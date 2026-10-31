//! C4 验收：修复器真模型 e2e（replace_bg / upscale fixer 链 + inpaint do 链）。
//!
//! 环境（容器内）：
//! - `DAM_TEST_MODELS_DIR=/models`（挂 /data，含 u2net /
//!   realesr-general-x4v3 / lama-fp32 三模型目录；缺 → 跳过）
//!
//! 判据（C4 卡验收「每插件独立验收」的模型侧落法）：
//! 1. replace_bg（guaranteed）：灰底图 white_edge fail → 抹主体铺白 → 复检 PASS，
//!    产物边角≈白、主体保持暗色；
//! 2. upscale（best_effort）：小图 size_min fail → x4v3 4× → 复检 PASS，产物 4 倍尺寸；
//! 3. inpaint（do 链）：box 相对矩形重绘，掩码区被改写、掩码外逐字节保真（paste-back）。

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

use rusqlite::Connection;
use svc_pipeline::compliance_queue::{compliance_enq, run_compliance_queue};

// ---------- 模型根（三模型符号链接，进程内一次性装配） ----------

/// 进程内一次性装配结果（三模型符号链接根 + 两个引擎的根注入）。
/// 用 `OnceLock` 把「建目录/挂链接/注入根」整体做成一次的：原先 `Once` 只守着注入，
/// 目录装配在 `Once` 外每个用例各做一遍（`remove_dir_all` + 重建同一临时根），
/// 并行跑时会把正在加载模型的用例的符号链接抽掉 —— 与卡 task 同源的并行踩踏。
static MODELS: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 首次调用装配模型根并返回；缺 `DAM_TEST_MODELS_DIR`/缺模型目录 → `None`（用例跳过）。
///
/// 这里不取 `ModelsRootScope`：本二进制只有这一处写根（进程内一次性），没有第二个写者；
/// 跨测试二进制由 cargo 串行执行。同二进制里若将来再加改根用例，必须改用
/// `engine_segment::ModelsRootScope` / `engine_restore::ModelsRootScope`。
fn models_root() -> Option<PathBuf> {
    MODELS
        .get_or_init(|| {
            let Ok(src) = std::env::var("DAM_TEST_MODELS_DIR") else {
                eprintln!("skip: DAM_TEST_MODELS_DIR 未设置");
                return None;
            };
            let src = PathBuf::from(src);
            let mut ok = true;
            for m in ["u2net", "realesr-general-x4v3", "lama-fp32"] {
                if !src.join(m).is_dir() {
                    eprintln!("skip: 模型目录缺失 {m}");
                    ok = false;
                }
            }
            if !ok {
                return None;
            }
            let root =
                std::env::temp_dir().join(format!("dam-c4-models-root-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("models")).unwrap();
            for m in ["u2net", "realesr-general-x4v3", "lama-fp32"] {
                #[cfg(unix)]
                std::os::unix::fs::symlink(src.join(m), root.join("models").join(m)).unwrap();
            }
            engine_segment::set_models_root(&root);
            engine_restore::set_models_root(&root);
            Some(root)
        })
        .clone()
}

// ---------- fixture（确定性合成图） ----------

fn write_jpg(path: &Path, w: u32, h: u32, paint: impl Fn(u32, u32) -> [u8; 3]) {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb(paint(x, y)));
    image::DynamicImage::ImageRgb8(img)
        .save_with_format(path, image::ImageFormat::Jpeg)
        .unwrap();
}

/// 灰底 220 + 中央暗色主体（u2net 显著性应收敛到主体）。
#[allow(dead_code)] // 保留：掩码夹具桩（当前断言路径未消费）
fn gray_with_subject(path: &Path, w: u32, h: u32) {
    write_jpg(path, w, h, |x, y| {
        let in_subject = x > w / 4 && x < w * 3 / 4 && y > h / 4 && y < h * 3 / 4;
        if in_subject {
            [40, 40, 45]
        } else {
            [220, 220, 220]
        }
    });
}

// ---------- 队列驱动（复制 C2 测试形态：in-memory DB + 直跑队列） ----------

struct Ctx {
    dir: PathBuf,
    conn: Connection,
}

impl Ctx {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("damc4-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        core_storage::migrations::migrate(&conn).unwrap();
        Self { dir, conn }
    }

    fn add_asset(&self, name: &str, paint: impl Fn(u32, u32) -> [u8; 3], w: u32, h: u32) -> i64 {
        let path = self.dir.join("src").join(name);
        write_jpg(&path, w, h, paint);
        self.conn
            .execute(
                "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
                 VALUES (NULL, ?1, 'image', ?2, 'jpg', 1, 0, 0)",
                rusqlite::params![path.to_string_lossy(), name],
            )
            .unwrap();
        let id = self.conn.last_insert_rowid();
        self.conn
            .execute(
                "INSERT INTO resource (assetId, role, ordinal, width, height) VALUES (?1, 'primary', 0, ?2, ?3)",
                rusqlite::params![id, w, h],
            )
            .unwrap();
        id
    }

    fn verdict_of(&self, asset: i64, rule: &str) -> (String, Option<String>) {
        self.conn
            .query_row(
                "SELECT COALESCE(verdict, CASE result WHEN 'skipped' THEN 'skipped' ELSE 'undetermined' END), evidence FROM rule_verdict WHERE assetId=?1 AND ruleId=?2
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![asset, rule],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .unwrap()
    }
}

#[test]
fn replace_bg_fixer_chain_with_real_u2net() {
    let Some(_root) = models_root() else {
        return;
    };
    let ctx = Ctx::new("rb");
    let id = ctx.add_asset(
        "IMG_bg.jpg",
        |x, y| {
            let subject = x > 80 && x < 240 && y > 60 && y < 180;
            if subject {
                [40, 40, 45]
            } else {
                [220, 220, 220]
            }
        },
        320,
        240,
    );
    let pack = core_config::Pack::from_yaml_strs(
        "c4-rb",
        "category: [x]\n",
        r#"
- id: R1bg
  name: 白底纯净度
  executor: classic
  check: {detector: white_edge, params: {max_dev: 8}}
  fixer: {type: replace_bg, tier: guaranteed, params: {bg_color: "ffffff"}}
  on_fail: auto_fix
"#,
        "steps:\n  - {kind: judge, rule: R1bg}\nrecheck_max: 1\n",
        "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1bg]\n",
    )
    .unwrap();
    let out_dir = ctx.dir.join("out");
    compliance_enq(
        &ctx.conn,
        &[id],
        pack.id.as_str(),
        Some(&out_dir.to_string_lossy()),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let report = run_compliance_queue(&ctx.conn, &pack, &cancel, &mut |_| {}).expect("run");
    assert_eq!(report.done, 1, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");

    // 灰底 220 → 初检 fail（dev=35 > 8）→ 修复后复检 PASS
    let (result, evidence) = ctx.verdict_of(id, "R1bg");
    assert_eq!(result, "pass", "evidence={evidence:?}");
    let ev: serde_json::Value = serde_json::from_str(&evidence.unwrap()).unwrap();
    assert_eq!(ev["fix"]["plugin"], "replace_bg");
    assert_eq!(ev["fix"]["tier"], "guaranteed");
    assert_eq!(ev["recheck"]["pass"], true);
    let fixed = ev["fix"]["output"]["path"].as_str().unwrap().to_string();
    // 产物像素：边角≈白、主体保持暗
    let (w, h, px) = core_media::rgb8(Path::new(&fixed)).unwrap();
    assert_eq!((w, h), (320, 240), "replace_bg 不改尺寸");
    let corner = (0..3).map(|i| px[i] as i32).sum::<i32>() / 3;
    assert!(corner > 245, "边角应≈白，实际 {corner}");
    let center = ((120 * w + 160) as usize) * 3;
    let cmean = (0..3).map(|i| px[center + i] as i32).sum::<i32>() / 3;
    assert!(cmean < 90, "主体应保持暗色，实际 {cmean}");
}

#[test]
fn upscale_fixer_chain_with_real_x4v3() {
    let Some(_root) = models_root() else {
        return;
    };
    let ctx = Ctx::new("up");
    let id = ctx.add_asset(
        "IMG_small.jpg",
        |x, y| [((x * 7 + y * 3) % 200 + 40) as u8, 90, 120],
        200,
        200,
    );
    let pack = core_config::Pack::from_yaml_strs(
        "c4-up",
        "category: [x]\n",
        r#"
- id: R1sz
  name: 最小尺寸 700
  executor: classic
  check: {detector: size_min, params: {min: 700}}
  fixer: {type: upscale, tier: best_effort}
  on_fail: auto_fix
"#,
        "steps:\n  - {kind: judge, rule: R1sz}\nrecheck_max: 1\n",
        "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, R1sz]\n",
    )
    .unwrap();
    let out_dir = ctx.dir.join("out");
    compliance_enq(
        &ctx.conn,
        &[id],
        pack.id.as_str(),
        Some(&out_dir.to_string_lossy()),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let report = run_compliance_queue(&ctx.conn, &pack, &cancel, &mut |_| {}).expect("run");
    assert_eq!(report.done, 1, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");

    // 200×150 → 初检 fail → 4× → 800×600 → 复检 PASS
    let (result, evidence) = ctx.verdict_of(id, "R1sz");
    assert_eq!(result, "pass", "evidence={evidence:?}");
    let ev: serde_json::Value = serde_json::from_str(&evidence.unwrap()).unwrap();
    assert_eq!(ev["fix"]["plugin"], "upscale");
    assert_eq!(ev["fix"]["tier"], "best_effort");
    assert_eq!(ev["fix"]["output"]["width"], 800);
    assert_eq!(ev["fix"]["output"]["height"], 800);
    let fixed = ev["fix"]["output"]["path"].as_str().unwrap().to_string();
    assert_eq!(
        core_media::image_size(Path::new(&fixed)).unwrap(),
        (800, 800)
    );
}

#[test]
fn inpaint_do_step_paste_back_with_real_lama() {
    let Some(_root) = models_root() else {
        return;
    };
    let ctx = Ctx::new("ip");
    // 均匀亮底 + 中央红块（box 覆盖红块；lama 补回亮底）
    let id = ctx.add_asset(
        "IMG_red.jpg",
        |x, y| {
            let red = x > 96 && x < 160 && y > 96 && y < 160;
            if red {
                [200, 30, 30]
            } else {
                [235, 235, 232]
            }
        },
        256,
        256,
    );
    let pack = core_config::Pack::from_yaml_strs(
        "c4-ip",
        "category: [x]\n",
        "[]",
        r#"
steps:
  - {kind: do, id: ip1, plugin: inpaint, params: {box: [0.35, 0.35, 0.65, 0.65]}}
recheck_max: 1
"#,
        "summary: [合格, 不合格, 未判定]\ndetail_columns: [imageSN, ip1]\n",
    )
    .unwrap();
    let out_dir = ctx.dir.join("out");
    compliance_enq(
        &ctx.conn,
        &[id],
        pack.id.as_str(),
        Some(&out_dir.to_string_lossy()),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let report = run_compliance_queue(&ctx.conn, &pack, &cancel, &mut |_| {}).expect("run");
    if report.done != 1 {
        // 响亮取证：job 行 lastError 原文（不上屏就排查不了 do 链错误）
        let err: Option<String> = ctx
            .conn
            .query_row(
                "SELECT lastError FROM job_queue WHERE kind='compliance' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap_or(None);
        panic!("inpaint job 未完成: {report:?} lastError={err:?}");
    }
    assert_eq!(report.failed, 0, "{report:?}");

    let (result, evidence) = ctx.verdict_of(id, "ip1");
    assert_eq!(result, "pass", "evidence={evidence:?}");
    let ev: serde_json::Value = serde_json::from_str(&evidence.unwrap()).unwrap();
    let out_path = ev["output"]["path"].as_str().unwrap().to_string();
    let (w, h, px) = core_media::rgb8(Path::new(&out_path)).unwrap();
    assert_eq!((w, h), (256, 256));
    // 掩码区（中心）不再是红色
    let center = ((128 * w + 128) as usize) * 3;
    assert!(
        (px[center] as i32) - (px[center + 1] as i32) < 60,
        "中心应被补绘（不再是纯红）：{} {} {}",
        px[center],
        px[center + 1],
        px[center + 2]
    );
    // paste-back：掩码外保真（角落 = 原色 ±3；buffer 级逐字节保真由 engine-restore
    // smoke 断言，此处产物经 JPEG 重编码有 ±1 量级 codec 抖动）
    let corner = ((8 * w + 8) as usize) * 3;
    for (got, expect) in [
        (px[corner], 235u8),
        (px[corner + 1], 235),
        (px[corner + 2], 232),
    ] {
        assert!(
            (i32::from(got) - i32::from(expect)).abs() <= 3,
            "掩码外必须保真: 角落=({}, {}, {}) 原色≈(235,235,232)",
            px[corner],
            px[corner + 1],
            px[corner + 2]
        );
    }
}

//! N6d `task` 装配验收：真 `pack-host` + 真能力网关 + **真插件进程**跑 imaging 包。
//!
//! 为什么不能只做 python 单测：装配错的地方（manifest 字段写错、`params.schema.json`
//! 宿主解析不了、`options.action` 没接到组 handler、行里混进 `verdict`）全都只在
//! **宿主那一侧**才炸 —— 装机期拒装或整批拒收，离作者最远。这里用宿主同一份实现验：
//!   1. `load_pack` 接受 manifest（`kinds=["process"]`、`params` 指向的 schema 能被
//!      `ParamSchema` 解析、`resources[]` 全在）；
//!   2. 参数合并的真实行为：默认层 → `action=resize`；规格层 `{action:"compress",
//!      compressMode:"quality"}` → `when` 不成立的分支（targetBytes 等）被剔掉；
//!   3. 六个动作里**不依赖模型**的三个（resize / watermark / rename）走真网关端到端：
//!      产物真落盘、尺寸对得上、行被 `result::parse_row(kinds=[Process])` 接受；
//!   4. 错误面：未知动作 → `-32003`，处理类回了 `verdict` → 宿主侧违例码
//!      `row_verdict_not_allowed`（本包不可能回，这条验的是宿主真在拦）。
//!
//! 依赖模型的两个动作（crop 要 u2net、compress 的目标体积档只用 encode 不依赖模型，
//! 但 crop 必须）不在本文件里假装验过：crop 的语料证据在
//! `packs/plugins/imaging/tests/corpus_geometry_e2e.py`（N6a 卡）。

use std::path::{Path, PathBuf};

use pack_host::params::ParamSchema;
use pack_host::result::{parse_row, PluginKind};
use pack_host::{Gateway, GatewayConfig, PackHost, PackHostConfig};
use serde_json::{json, Value};

const PACK: &str = "packs/plugins/imaging";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn pack_dir() -> PathBuf {
    repo_root().join(PACK)
}

fn make_png(path: &Path, w: i32, h: i32) {
    let mut rgb = vec![200u8; (w as usize) * (h as usize) * 3];
    for y in h / 4..3 * h / 4 {
        for x in w / 5..4 * w / 5 {
            let i = ((y * w + x) * 3) as usize;
            rgb[i] = 30;
            rgb[i + 1] = 60;
            rgb[i + 2] = 200;
        }
    }
    core_media::encode_rgb_to(w, h, &rgb, path, None).expect("写测试图失败");
}

/// 起「真网关 + 真插件」：网关由本进程提供（随机端口 + 一次性令牌），插件是子进程。
///
/// 返回的 `Gateway` **必须由调用方持有到测试结束**：`Gateway::drop` 会 stop 监听线程，
/// 提前丢掉它 = 插件调网关时 connection refused（表现为 `gateway_error(None)`，
/// 因为 PluginError 拿不到 HTTP 状态也拿不到 error 字段）。
fn open_host(tag: &str) -> (tempfile::TempDir, PathBuf, Gateway, PackHost) {
    let tmp = tempfile::tempdir().expect("临时根");
    let work = tmp.path().join(format!("work-{tag}"));
    std::fs::create_dir_all(&work).expect("工作目录");
    let manifest = pack_host::load_pack(&pack_dir())
        .expect("imaging manifest 应可加载")
        .0;
    let gw =
        Gateway::start(GatewayConfig::from_manifest(&manifest, &work)).expect("真网关启动失败");
    let mut cfg = PackHostConfig::new(gw.base_url(), gw.token(), &work);
    cfg.runtime_python = Some(PathBuf::from("python3"));
    let host = PackHost::open(pack_dir(), cfg).expect("插件宿主打开失败");
    (tmp, work, gw, host)
}

fn rows_of(reply: &Value) -> Vec<Value> {
    let rows = reply
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(!rows.is_empty(), "judge 必须回非空 rows：{reply}");
    rows
}

// ---------------------------------------------------------------------------
// 1. manifest + 参数 schema：宿主解析器说了算
// ---------------------------------------------------------------------------

#[test]
fn manifest_and_params_accepted_by_host() {
    let (manifest, identity) = pack_host::load_pack(&pack_dir()).expect("manifest 应被宿主接受");
    assert_eq!(manifest.id, "com.soujpg.pack.imaging");
    assert_eq!(manifest.api_version, "1.1");
    assert_eq!(manifest.effective_kinds(), vec![PluginKind::Process]);
    assert!(manifest.is_process() && !manifest.is_judge());
    assert_eq!(
        manifest.missing_resources(&pack_dir()),
        Vec::<String>::new()
    );
    assert!(
        identity.plugin_hash.len() == 64,
        "pluginHash 必须是 manifest 原文 sha256"
    );
    let declared = manifest.params.as_deref().expect("必须声明 params");
    assert_eq!(declared, "resources/params.schema.json");

    // 宿主安装期走的同一条路径：文件必须在 + 能被 ParamSchema 解析
    let schema = ParamSchema::load_declared(&pack_dir(), Some(declared))
        .expect("宿主参数解析器必须接受")
        .expect("声明了 params 就应有 schema");
    assert_eq!(schema.schema_version(), 1);
    let groups: Vec<&str> = schema.groups().iter().map(|g| g.key.as_str()).collect();
    assert_eq!(groups, vec!["basic", "geometry", "codec", "overlay"]);
    for key in [
        "action",
        "resizeMode",
        "convertFormat",
        "watermarkText",
        "renameTemplate",
    ] {
        assert!(schema.properties().contains_key(key), "缺参数 {key}");
    }

    // 默认层：动作默认 resize，且 crop/compress 的条件分支被 prune 掉
    let effective = schema
        .resolve(&[("defaults", &serde_json::Map::new())])
        .expect("默认层合并");
    assert_eq!(
        effective.get("action").and_then(Value::as_str),
        Some("resize")
    );
    assert!(effective.contains_key("resizeLongEdge"));
    // 组内 `when` 真在剪枝：cropAspect 默认 1:1，→ cropAspectCustom（when custom）不该出现
    assert!(!effective.contains_key("cropAspectCustom"));
    // 但**没有跨组门控**（卡面硬边界）：别的动作的参数照样在，靠 group 分区而不是隐藏
    for key in [
        "compressTargetBytes",
        "watermarkText",
        "renameTemplate",
        "convertFormat",
    ] {
        assert!(
            effective.contains_key(key),
            "不该按动作隐藏 {key}（跨组 when 禁止）"
        );
    }

    // 规格层：选 compress + quality 档 → 体积档参数必须被剔除（`when` 真在生效）
    let mut spec = serde_json::Map::new();
    spec.insert("action".into(), json!("compress"));
    spec.insert("compressMode".into(), json!("quality"));
    let effective = schema.resolve(&[("spec", &spec)]).expect("规格层合并");
    assert_eq!(
        effective.get("action").and_then(Value::as_str),
        Some("compress")
    );
    assert_eq!(
        effective.get("compressQuality").and_then(Value::as_i64),
        Some(82)
    );
    assert!(!effective.contains_key("compressTargetBytes"));
    assert!(!effective.contains_key("compressPreferredQuality"));
    // 跨组不受影响：compress 的规格里 resize 参数仍在（插件按 action 只用自己的那组）
    assert!(effective.contains_key("resizeLongEdge"));
    assert!(effective.contains_key("watermarkText"));

    // 越界动作在合并期就被宿主挡住（不靠插件自觉）
    let mut bad = serde_json::Map::new();
    bad.insert("action".into(), json!("sharpen"));
    assert!(
        schema.resolve(&[("bad", &bad)]).is_err(),
        "未知动作必须合并期报错"
    );
}

// ---------------------------------------------------------------------------
// 2. 端到端（真网关 + 真插件）：三个不依赖模型的动作
// ---------------------------------------------------------------------------

#[test]
fn resize_end_to_end_through_real_gateway() {
    let (tmp, work, _gw, host) = open_host("resize");
    let img = tmp.path().join("src.png");
    make_png(&img, 1600, 1200);

    let reply = host
        .call(
            "judge",
            json!({
                "job_id": "job-resize",
                "asset": {"id": "asset-1", "path": img.to_string_lossy(),
                          "mime": "image/png", "width": 1600, "height": 1200},
                "rules": null,
                "options": {"action": "resize", "resizeMode": "long_edge",
                            "resizeLongEdge": 800, "workdir": work.to_string_lossy()},
            }),
        )
        .expect("judge 不应报协议错");

    let rows = rows_of(&reply);
    let row = &rows[0];
    assert_eq!(
        row.get("result").and_then(Value::as_str),
        Some("ok"),
        "{row}"
    );
    assert!(
        row.get("verdict").is_none(),
        "处理类回 verdict 会被整批拒收"
    );
    // 宿主侧行校验（同一份实现，违例即拒收）
    let parsed = parse_row(&[PluginKind::Process], row).expect("宿主必须接受该行");
    assert_eq!(parsed.asset_id, "asset-1");

    let fields = row.get("fields").expect("fields");
    assert_eq!(fields.get("action").and_then(Value::as_str), Some("resize"));
    let out = fields
        .get("outPath")
        .and_then(Value::as_str)
        .expect("必须回产物路径");
    assert!(Path::new(out).exists(), "产物必须真落盘：{out}");
    assert!(
        out.starts_with(work.to_string_lossy().as_ref()),
        "产物必须落宿主 workdir"
    );
    let reply = core_media::image_size(Path::new(out)).expect("产物可解码");
    assert_eq!(reply, (800, 600), "长边 1600→800、等比 4:3 应得 800x600");
}

#[test]
fn watermark_end_to_end_through_real_gateway() {
    let (tmp, work, _gw, host) = open_host("watermark");
    let img = tmp.path().join("src.png");
    make_png(&img, 800, 600);

    let reply = host
        .call(
            "judge",
            json!({
                "job_id": "job-wm",
                "asset": {"id": "asset-2", "path": img.to_string_lossy()},
                "options": {"action": "watermark", "watermarkText": "DAMDesk",
                            "watermarkAnchor": "bottom_right", "watermarkColor": "#ff0000",
                            "watermarkOpacity": 0.8, "workdir": work.to_string_lossy()},
            }),
        )
        .expect("judge 不应报协议错");
    let row = &rows_of(&reply)[0];
    assert_eq!(
        row.get("result").and_then(Value::as_str),
        Some("ok"),
        "{row}"
    );
    parse_row(&[PluginKind::Process], row).expect("宿主必须接受该行");
    let fields = row.get("fields").expect("fields");
    assert_eq!(
        fields.get("action").and_then(Value::as_str),
        Some("watermark")
    );
    let out = fields
        .get("outPath")
        .and_then(Value::as_str)
        .expect("产物路径");
    // 叠加不是变形：产物尺寸必须等于源尺寸
    assert_eq!(
        core_media::image_size(Path::new(out)).expect("产物可解码"),
        (800, 600)
    );
}

#[test]
fn rename_end_to_end_through_real_gateway() {
    let (tmp, work, _gw, host) = open_host("rename");
    let img = tmp.path().join("IMG_0007.png");
    make_png(&img, 640, 480);

    let call = |job: &str| {
        host.call(
            "judge",
            json!({
                "job_id": job,
                "asset": {"id": "asset-3", "path": img.to_string_lossy()},
                "options": {"action": "rename", "renameTemplate": "{name}_{width}x{height}",
                            "workdir": work.to_string_lossy()},
            }),
        )
        .expect("judge 不应报协议错")
    };

    let first = call("job-rename");
    let row = &rows_of(&first)[0];
    assert_eq!(
        row.get("result").and_then(Value::as_str),
        Some("ok"),
        "{row}"
    );
    parse_row(&[PluginKind::Process], row).expect("宿主必须接受该行");
    let fields = row.get("fields").expect("fields");
    assert_eq!(
        fields.get("assetName").and_then(Value::as_str),
        Some("IMG_0007_640x480.png")
    );
    assert!(
        fields.get("outPath").is_none(),
        "rename 只出名字，不出产物（宿主负责落名）"
    );

    // 同一 job 内重名 → 尾号 -2（批内确定性）
    let second = call("job-rename");
    let row2 = &rows_of(&second)[0];
    assert_eq!(
        row2.pointer("/fields/assetName").and_then(Value::as_str),
        Some("IMG_0007_640x480-2.png")
    );
}

#[test]
fn convert_end_to_end_through_real_gateway() {
    let (tmp, work, _gw, host) = open_host("convert");
    let img = tmp.path().join("src.png");
    make_png(&img, 600, 400);

    let reply = host
        .call(
            "judge",
            json!({
                "job_id": "job-convert",
                "asset": {"id": "asset-5", "path": img.to_string_lossy()},
                "options": {"action": "convert", "convertFormat": "webp",
                            "convertQuality": 80, "workdir": work.to_string_lossy()},
            }),
        )
        .expect("judge 不应报协议错");
    let row = &rows_of(&reply)[0];
    assert_eq!(
        row.get("result").and_then(Value::as_str),
        Some("ok"),
        "{row}"
    );
    parse_row(&[PluginKind::Process], row).expect("宿主必须接受该行");
    let fields = row.get("fields").expect("fields");
    assert_eq!(
        fields.get("action").and_then(Value::as_str),
        Some("convert")
    );
    let out = fields
        .get("outPath")
        .and_then(Value::as_str)
        .expect("产物路径");
    assert!(out.ends_with(".webp"), "产物扩展名必须跟目标格式：{out}");
    assert_eq!(
        core_media::image_size(Path::new(out)).expect("产物可解码"),
        (600, 400)
    );
    // 源图无透明像素 → 宿主如实回 absent（不假装 kept）
    assert_eq!(
        fields.get("alpha").and_then(Value::as_str),
        Some("absent"),
        "{fields}"
    );
}

#[test]
fn compress_quality_mode_end_to_end_through_real_gateway() {
    let (tmp, work, _gw, host) = open_host("compress");
    let img = tmp.path().join("src.png");
    make_png(&img, 1200, 900);

    let reply = host
        .call(
            "judge",
            json!({
                "job_id": "job-compress",
                "asset": {"id": "asset-6", "path": img.to_string_lossy()},
                "options": {"action": "compress", "compressMode": "quality",
                            "compressQuality": 55, "compressFormat": "jpg",
                            "workdir": work.to_string_lossy()},
            }),
        )
        .expect("judge 不应报协议错");
    let row = &rows_of(&reply)[0];
    assert_eq!(
        row.get("result").and_then(Value::as_str),
        Some("ok"),
        "{row}"
    );
    parse_row(&[PluginKind::Process], row).expect("宿主必须接受该行");
    let fields = row.get("fields").expect("fields");
    assert_eq!(
        fields.get("action").and_then(Value::as_str),
        Some("compress")
    );
    let out = fields
        .get("outPath")
        .and_then(Value::as_str)
        .expect("产物路径");
    let out_bytes = std::fs::metadata(out).expect("产物必须落盘").len();
    let in_bytes = std::fs::metadata(&img).expect("源图").len();
    assert!(out_bytes > 0 && out_bytes != in_bytes, "产物应真被重编码");
    assert_eq!(
        core_media::image_size(Path::new(out)).expect("产物可解码"),
        (1200, 900)
    );
}

/// `crop` 要 u2net（唯一依赖模型的装配动作）。两支都验，**不许静默**：
///   - 模型在盘（容器 `/models`）→ 走 ok 支：主体完整落在产物内 + 产物真落盘；
///   - 模型不在 → 必须回 `-32001 capability_missing` 且**没有产物**（不假装「裁好了」）。
#[test]
fn crop_end_to_end_or_capability_missing() {
    let (tmp, work, _gw, host) = open_host("crop");
    let img = tmp.path().join("src.png");
    make_png(&img, 1200, 900);

    let reply = host
        .call(
            "judge",
            json!({
                "job_id": "job-crop",
                "asset": {"id": "asset-7", "path": img.to_string_lossy()},
                "options": {"action": "crop", "cropAspect": "1:1", "cropAnchor": "center",
                            "cropPadding": 0.05, "workdir": work.to_string_lossy()},
            }),
        )
        .expect("judge 不应报协议错");
    let row = rows_of(&reply).remove(0);
    let result = row.get("result").and_then(Value::as_str).unwrap_or("");
    let fields = row.get("fields").cloned().unwrap_or_else(|| json!({}));
    match result {
        "ok" => {
            parse_row(&[PluginKind::Process], &row).expect("宿主必须接受该行");
            let out = fields
                .get("outPath")
                .and_then(Value::as_str)
                .expect("产物路径");
            let (w, h) = core_media::image_size(Path::new(out)).expect("产物可解码");
            assert_eq!(
                w, h,
                "cropAspect=1:1 → 产物必须是正方形（补边后），实得 {w}x{h}"
            );
            assert_eq!(
                row.pointer("/evidence/subjectCovered")
                    .and_then(Value::as_bool),
                Some(true),
                "主体必须完整落在产物内：{row}"
            );
            println!("crop：ok 支（模型在盘），产物 {w}x{h}");
        }
        "failed" => {
            let code = row
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("");
            assert_eq!(
                code, "-32001",
                "模型缺失必须回能力缺失，不许回别的借口：{row}"
            );
            assert!(
                !Path::new(work.to_string_lossy().as_ref())
                    .join("src.crop.png")
                    .exists()
                    && fields.get("outPath").is_none(),
                "能力缺失时不许留下产物：{row}"
            );
            println!("crop：capability_missing 支（本机无 u2net）");
        }
        other => panic!("crop 结果既不是 ok 也不是 failed：{other} {row}"),
    }
}

// ---------------------------------------------------------------------------
// 3. 错误面：未知动作真被 -32003 挡住（不是「静默跑默认动作」）
// ---------------------------------------------------------------------------

#[test]
fn unknown_action_is_rejected_with_32003() {
    let (tmp, work, _gw, host) = open_host("bad-action");
    let img = tmp.path().join("src.png");
    make_png(&img, 320, 240);
    let err = host
        .call(
            "judge",
            json!({
                "job_id": "job-bad",
                "asset": {"id": "asset-4", "path": img.to_string_lossy()},
                "options": {"action": "sharpen", "workdir": work.to_string_lossy()},
            }),
        )
        .expect_err("未知动作必须报错");
    let text = err.to_string();
    assert!(text.contains("32003") || text.contains("sharpen"), "{text}");
}

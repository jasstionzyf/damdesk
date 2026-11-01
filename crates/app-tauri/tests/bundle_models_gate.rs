//! GH-7 打包断言（字节级）：安装包只准带检索必需塔 siglip2，且权重必须与
//! `engine_embed::model_store::manifests()` 声明逐项一致（bytes + sha256）。
//!
//! 与 `scripts/models_bundle_gate.sh`（结构级，beforeBundleCommand 挂载）同守一道门，
//! 这一层管字节：防止 stage 错版本/半截文件进包 —— 下载端按 sha256 锁定，包内置错字节
//! 会造成「装了但校验不过」的静默不可用。
//!
//! 门控：env `DAM_RELEASE_GATE=1` 才强制校验（release.yml 与本地出包脚本设置）；
//! 未设 → SKIP。为什么：CI/开发机 checkout 没有 gitignored 的权重目录是常态，
//! 无条件跑会假红；而 release 流程必然先 stage 权重再设本开关，不会漏跑。
//!
//! 用法：`DAM_RELEASE_GATE=1 cargo test -p app-tauri --test bundle_models_gate`

use std::path::{Path, PathBuf};
use std::process::Command;

use engine_embed::model_store::{manifest_for, sha256_file};

const BUNDLED: &str = "siglip2-base-p224-onnx";

fn gate_enforced() -> bool {
    std::env::var("DAM_RELEASE_GATE").as_deref() == Ok("1")
}

fn resources_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("resources")
}

/// 递归收集路径名含 dinov3（不区分大小写）的文件/目录 —— 红线：一处命中即失败。
fn find_dinov3(dir: &Path, hits: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.to_ascii_lowercase().contains("dinov3") {
            hits.push(p.clone());
        }
        if p.is_dir() {
            find_dinov3(&p, hits);
        }
    }
}

#[test]
fn gh7_bundle_is_siglip2_only_and_sha256_locked() {
    if !gate_enforced() {
        // 不算假绿：CI/开发环境没有权重目录是常态；release 流程必设 DAM_RELEASE_GATE=1。
        eprintln!(
            "SKIP  bundle gate：未设 DAM_RELEASE_GATE=1（非 release 打包环境，权重目录缺省是常态）"
        );
        return;
    }

    let res = resources_dir();
    let models = res.join("models");
    let bundled_dir = models.join(BUNDLED);

    // ---- 红线 1：resources 下任何路径不得出现 dinov3（Meta 许可禁止随包再分发） ----
    let mut hits = Vec::new();
    find_dinov3(&res, &mut hits);
    assert!(
        hits.is_empty(),
        "打包红线：resources/ 出现 dinov3 路径（GH-5 摘除对象，禁止随包）：{hits:?}"
    );

    // ---- 结构：models/ 下有且只有一个条目 = siglip2-base-p224-onnx ----
    assert!(
        bundled_dir.is_dir(),
        "{bundled_dir:?} 不存在 —— 先 stage siglip2 权重再出包"
    );
    let mut entries: Vec<String> = std::fs::read_dir(&models)
        .expect("读 resources/models")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![BUNDLED.to_string()],
        "lite 安装包只准带检索必需塔 {BUNDLED}；其余权重走 model_store 分发（GH-9），混入即构建失败"
    );

    // ---- 字节级：manifest 声明的每个文件，bytes + sha256 必须与盘上一致 ----
    // （名字用字面量：core-config 不在 dev-deps；与 shell.rs BUNDLED_MODELS 同源，dt5 单测钉住一致性）
    let manifest =
        manifest_for(BUNDLED).expect("model_store::manifests() 必须声明 siglip2（内置检索塔）");
    assert!(!manifest.files.is_empty(), "siglip2 manifest 文件表为空");
    for f in &manifest.files {
        let p = bundled_dir.join(&f.name);
        let (bytes, sha) =
            sha256_file(&p).unwrap_or_else(|e| panic!("内置塔文件 {} 不可读：{e}", p.display()));
        assert_eq!(
            bytes,
            f.bytes,
            "{} 字节数 ≠ model_store.rs 声明（stage 的权重版本不对或文件截断）",
            p.display()
        );
        assert_eq!(
            sha,
            f.sha256,
            "{} sha256 ≠ model_store.rs 声明 —— 进包必判损坏（下载端按 sha256 锁定）",
            p.display()
        );
    }

    // ---- 附带：打包配置本身必须只剩一条模型资源映射（与门禁脚本口径互证） ----
    let conf = include_str!("../tauri.conf.json");
    let model_entries = conf.matches("\"resources/models/").count();
    assert_eq!(
        model_entries, 1,
        "tauri.conf.json bundle.resources 的模型映射必须只剩 siglip2 一条，实际 {model_entries} 条"
    );
    assert!(
        conf.contains(&format!("\"resources/models/{BUNDLED}/**/*\"")),
        "tauri.conf.json 缺 siglip2 的 resources 映射（包里会没有权重）"
    );

    eprintln!(
        "PASS  bundle gate：models 仅含 {BUNDLED}，manifest {} 个文件 bytes+sha256 全对齐，无 dinov3 路径",
        manifest.files.len()
    );
}

/// 打包机冒烟：门禁脚本体本身可执行（release.yml 在 cargo 之前也会跑它；这里防脚本被误删/改坏）。
#[test]
fn gh7_gate_script_exists_and_runs_red_on_dirty_staging() {
    // 只验证脚本存在且可执行语法完整（bash -n）；真正的红/绿在打包机上跑。
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/models_bundle_gate.sh")
        .canonicalize()
        .expect("scripts/models_bundle_gate.sh 不存在");
    let out = Command::new("bash")
        .arg("-n")
        .arg(&script)
        .output()
        .expect("运行 bash -n");
    assert!(
        out.status.success(),
        "models_bundle_gate.sh 语法损坏：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

//! 真模型分发集成测试：sha256 锁定的 manifest × 真模型文件 × file:// 分发源。
//!
//! 为什么真跑：D3 验收 2 要「删模型文件 → 触发下载 → 落盘 sha256 校验通过」。
//! 这里用**真 manifest（真字节数 + 真 sha256）**指向真 SigLIP2 文件（symlink 布局成
//! url_path 形状），因此下载 + 校验通过 = 线上首启下载路径本身已验证（唯一差别只是 URL 协议）。
//!
//! 环境：DAM_TEST_MODELS=/models/siglip2-base-p224-onnx（真模型目录；缺 → 显式失败）。

use std::path::{Path, PathBuf};

use engine_embed::model_store::{manifest_for, siglip2_base_p224, ModelStore};
use engine_embed::EmbedError;

fn real_model_dir() -> PathBuf {
    let dir = std::env::var("DAM_TEST_MODELS")
        .unwrap_or_else(|_| "/models/siglip2-base-p224-onnx".to_string());
    let p = PathBuf::from(&dir);
    assert!(
        p.join("vision.onnx").is_file()
            && p.join("text.onnx").is_file()
            && p.join("tokenizer.json").is_file(),
        "真模型三件套缺失：{dir}（见 reports/D3-inference.md 的环境准备）"
    );
    p
}

fn tmp_root(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dam-real-dl-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 把真模型文件 symlink 成 manifest.url_path 要求的目录形状（onnx/vision_model_fp16.onnx ...）。
fn symlink_source(root: &Path, models_dir: &Path) -> PathBuf {
    let src = root.join("hf-src");
    std::fs::create_dir_all(src.join("onnx")).unwrap();
    let m = siglip2_base_p224();
    for f in &m.files {
        let dst = src.join(&f.url_path);
        #[cfg(unix)]
        std::os::unix::fs::symlink(models_dir.join(&f.name), &dst).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(models_dir.join(&f.name), &dst).unwrap();
    }
    src
}

/// 验收 2（真文件版）：整模型下载 → sha256 全绿；删一个文件 → 只重下该文件；再删 → force 全量重下。
#[test]
fn downloads_real_siglip2_files_and_verifies_locked_sha256() {
    let models_dir = real_model_dir();
    let root = tmp_root("full");
    let src = symlink_source(&root, &models_dir);
    let manifest = manifest_for("siglip2-base-p224-onnx").expect("manifest");
    assert_eq!(manifest.files.len(), 3);
    println!(
        "manifest: {} files, {} bytes (vision {} / text {} / tokenizer {})",
        manifest.files.len(),
        manifest.total_bytes(),
        manifest.file("vision.onnx").unwrap().bytes,
        manifest.file("text.onnx").unwrap().bytes,
        manifest.file("tokenizer.json").unwrap().bytes
    );

    let store = ModelStore::new(root.join("models"), format!("file://{}", src.display()));
    assert!(!store.is_complete(&manifest));

    // 第一次：3 个文件全下（真 785MB 流式 sha256）
    let mut events = Vec::new();
    let outcome = store
        .download(&manifest, false, &mut |e| events.push(e))
        .unwrap();
    println!(
        "first download: downloaded={} skipped={} bytes={}",
        outcome.downloaded, outcome.skipped, outcome.totalBytes
    );
    assert_eq!((outcome.downloaded, outcome.skipped), (3, 0));
    assert_eq!(outcome.totalBytes, manifest.total_bytes());
    assert!(store.is_complete(&manifest));

    // 落盘 sha256 全绿（真 manifest 锁定的哈希）
    let checks = store.verify(&manifest).unwrap();
    for c in &checks {
        println!(
            "verify {}: bytes={} ok={} shaOk={:?}",
            c.name, c.bytes, c.present, c.shaOk
        );
    }
    assert!(
        checks.iter().all(|c| c.shaOk == Some(true)),
        "sha 校验必须全绿"
    );
    assert!(checks.iter().all(|c| c.present));
    assert_eq!(
        checks.iter().map(|c| c.bytes).sum::<u64>(),
        manifest.total_bytes()
    );

    // 删一个文件 → 重下只补这一个（幂等续下）
    let victim = store.file_path(&manifest, manifest.file("tokenizer.json").unwrap());
    std::fs::remove_file(&victim).unwrap();
    assert!(!store.is_complete(&manifest));
    let mut events2 = Vec::new();
    let o2 = store
        .download(&manifest, false, &mut |e| events2.push(e))
        .unwrap();
    println!(
        "after delete: downloaded={} skipped={}",
        o2.downloaded, o2.skipped
    );
    assert_eq!((o2.downloaded, o2.skipped), (1, 2));
    assert!(store
        .verify(&manifest)
        .unwrap()
        .iter()
        .all(|c| c.shaOk == Some(true)));

    // force → 全量重下
    let o3 = store.download(&manifest, true, &mut |_| {}).unwrap();
    println!("force: downloaded={} skipped={}", o3.downloaded, o3.skipped);
    assert_eq!((o3.downloaded, o3.skipped), (3, 0));

    let _ = std::fs::remove_dir_all(&root);
}

/// 损坏文件必须被拒（篡改后同字节数不同内容 → sha 不符），且不留 .part 残骸。
#[test]
fn tampered_real_file_is_rejected() {
    let models_dir = real_model_dir();
    let root = tmp_root("tamper");
    let src = symlink_source(&root, &models_dir);
    let manifest = manifest_for("siglip2-base-p224-onnx").unwrap();
    let store = ModelStore::new(root.join("models"), format!("file://{}", src.display()));
    store.download(&manifest, false, &mut |_| {}).unwrap();

    // 篡改已落盘文件：同长度，内容不同
    let p = store.file_path(&manifest, manifest.file("vision.onnx").unwrap());
    let n = std::fs::metadata(&p).unwrap().len();
    let mut f = std::fs::File::create(&p).unwrap();
    use std::io::Write;
    f.write_all(&vec![0u8; n as usize]).unwrap();
    drop(f);
    assert!(
        store.is_complete(&manifest),
        "快路径（只看字节数）看不出篡改"
    );
    let checks = store.verify(&manifest).unwrap();
    let v = checks.iter().find(|c| c.name == "vision.onnx").unwrap();
    assert_eq!(v.shaOk, Some(false), "verify 必须发现篡改");

    // verify 失败后再下载：字节数对 → 会被 skip（sha 只在完整下载时强校验），
    // 所以 force 重建才是修复路径 —— 这里断言 force 能修好。
    let o = store.download(&manifest, true, &mut |_| {}).unwrap();
    assert_eq!(o.downloaded, 3);
    assert!(store
        .verify(&manifest)
        .unwrap()
        .iter()
        .all(|c| c.shaOk == Some(true)));

    let _ = std::fs::remove_dir_all(&root);
}

/// 分发源缺文件 → 明确 Http 错误 + 不留 .part（半截文件永不进模型目录）。
#[test]
fn missing_source_file_leaves_no_part_file() {
    let root = tmp_root("partial");
    let empty_src = root.join("empty-src");
    std::fs::create_dir_all(&empty_src).unwrap();
    let manifest = siglip2_base_p224();
    let store = ModelStore::new(
        root.join("models"),
        format!("file://{}", empty_src.display()),
    );
    let err = store
        .download(&manifest, false, &mut |_| {})
        .expect_err("must fail");
    assert!(matches!(err, EmbedError::Http(_)), "got {err:?}");
    let part = store
        .file_path(&manifest, manifest.file("vision.onnx").unwrap())
        .with_extension("part");
    assert!(!part.exists(), "半截 .part 必须被清掉：{}", part.display());
    let _ = std::fs::remove_dir_all(&root);
}

//! 模型分发验收工具（原始命令 + 输出直接进 reports/D3-inference.md 验收 2）。
//!
//! 用法：
//!   # 本地/内网 file:// 分发源（容器内用真模型验证 sha256 锁定）
//!   cargo run -p engine-embed --example model_fetch -- /tmp/dam-models file:///tmp/hf-src
//!   # 真实镜像源（首启静默下载路径；785MB，走容器代理）
//!   cargo run -p engine-embed --example model_fetch -- /tmp/dam-models
//!   # 选项
//!   --force          全部重下（忽略已在盘的文件）
//!   --delete <file>  先删掉目标目录里的某个文件（模拟"删模型文件 → 触发下载"）
//!   --verify-only    只做 sha256 全量校验，不下载
//!
//! 退出码：0 = 全绿；非 0 = 失败（sha 不符 / 下载错误）。

use std::path::PathBuf;

use core_config::DEFAULT_EMBEDDER;
use engine_embed::model_store::{manifest_for, ModelStore};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut force = false;
    let mut verify_only = false;
    let mut delete: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.drain(..);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--force" => force = true,
            "--verify-only" => verify_only = true,
            "--delete" => delete = it.next(),
            other => positional.push(other.to_string()),
        }
    }
    let root = positional
        .first()
        .cloned()
        .unwrap_or_else(|| "/tmp/dam-models".to_string());
    let base_url = positional.get(1).cloned().unwrap_or_default();

    let manifest = manifest_for(DEFAULT_EMBEDDER).ok_or("no manifest for default embedder")?;
    let store = ModelStore::new(PathBuf::from(&root), base_url.clone());
    println!("model      : {}", manifest.name);
    println!("models root: {}", store.root().display());
    println!("base url   : {}", store.base_url());
    println!(
        "manifest   : {} files, {} bytes total",
        manifest.files.len(),
        manifest.total_bytes()
    );
    for f in &manifest.files {
        println!(
            "  - {} <= {} ({} bytes, sha256 {})",
            f.name, f.url_path, f.bytes, f.sha256
        );
    }

    if let Some(name) = &delete {
        let f = manifest
            .file(name)
            .ok_or(format!("manifest has no file named {name}"))?;
        let p = store.file_path(&manifest, f);
        if p.exists() {
            std::fs::remove_file(&p)?;
            println!("\n[delete] removed {}", p.display());
        } else {
            println!("\n[delete] {} not present (nothing to remove)", p.display());
        }
    }

    println!("\n-- status before --");
    print_status(&store, &manifest)?;

    if !verify_only {
        println!("\n-- downloading --");
        let mut last = String::new();
        let outcome = store.download(&manifest, force, &mut |e| {
            let line = format!(
                "  {:<11} {:<16} {}/{} files  {} / {} bytes{}",
                e.status,
                e.file,
                e.completedFiles,
                e.totalFiles,
                e.doneBytes,
                e.totalBytes,
                e.error
                    .as_ref()
                    .map(|x| format!("  ERR: {x}"))
                    .unwrap_or_default()
            );
            if line != last {
                println!("{line}");
                last = line;
            }
        })?;
        println!(
            "\nresult: downloaded={} skipped={} bytes={}",
            outcome.downloaded, outcome.skipped, outcome.totalBytes
        );
    }

    println!("\n-- verify (sha256) --");
    let checks = store.verify(&manifest)?;
    let mut all_ok = true;
    for c in &checks {
        let ok = c.shaOk == Some(true) && c.present;
        all_ok &= ok;
        println!(
            "  {:<16} present={:<5} bytes={:<12} expect_bytes={:<12} shaOk={:?}\n    expect {}\n    actual {}",
            c.name,
            c.present,
            c.bytes,
            c.expectedBytes,
            c.shaOk,
            c.expectedSha256,
            c.sha256.clone().unwrap_or_else(|| "-".into())
        );
    }
    println!(
        "\nVERDICT: {}",
        if all_ok {
            "PASS (all sha256 match)"
        } else {
            "FAIL"
        }
    );
    if !all_ok {
        std::process::exit(2);
    }
    Ok(())
}

fn print_status(
    store: &ModelStore,
    manifest: &engine_embed::model_store::ModelManifest,
) -> Result<(), Box<dyn std::error::Error>> {
    for c in store.status(manifest) {
        println!(
            "  {:<16} present={:<5} bytes={:<12} expect={}",
            c.name, c.present, c.bytes, c.expectedBytes
        );
    }
    println!("  complete={}", store.is_complete(manifest));
    Ok(())
}

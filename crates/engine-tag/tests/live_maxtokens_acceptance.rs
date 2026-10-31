//! 真机验收（卡 task）：max_tokens 截断 → 默认 2048 + 解析层补齐 + 失败分类。
//!
//! 用**真实** OpenAI 兼容 VL 端点（默认 DeepSeek，与 WIN-E2E 真机同一条路径）跑一批
//! 中文图，比较两个口径：
//!   1. 默认 max_tokens（2048）→ 期望 0 失败；
//!   2. 旧默认 1024（模拟真机现场）→ 记录截断/补齐/失败分类，验证不会静默丢图。
//!
//! 默认 `#[ignore]`（要外网 + Key），显式跑：
//! ```sh
//! DAM_LIVE_TAG_KEY=sk-... DAM_LIVE_TAG_MODEL=deepseek-flash \
//! DAM_LIVE_TAG_CORPUS=/data \
//! cargo test -p engine-tag --test live_maxtokens_acceptance -- --ignored --nocapture
//! ```
//! 可选：DAM_LIVE_TAG_LIMIT（默认 25，卡要求 20+）、DAM_LIVE_TAG_PROXY（默认直连）、
//! DAM_LIVE_TAG_BASE（默认 https://api.deepseek.com/v1）。

use std::path::{Path, PathBuf};

use core_config::{load_registry, TagSettings};
use engine_tag::openai_compat::OpenAiCompatTagProvider;
use engine_tag::{TagError, TagProvider};

fn spec() -> core_config::ModelSpec {
    load_registry(None)
        .unwrap()
        .models
        .into_iter()
        .find(|m| m.kind == core_config::SpecKind::Tagging)
        .expect("registry 里要有 tagging spec")
}

fn corpus(limit: usize) -> Vec<PathBuf> {
    let dir = std::env::var("DAM_LIVE_TAG_CORPUS")
        .unwrap_or_else(|_| "/data".into());
    let mut imgs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读语料目录 {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()),
                Some(ext) if ext == "jpg" || ext == "jpeg" || ext == "png"
            )
        })
        .collect();
    imgs.sort();
    imgs.truncate(limit);
    imgs
}

fn provider(max_tokens: u32) -> OpenAiCompatTagProvider {
    let key = std::env::var("DAM_LIVE_TAG_KEY").expect("需要 DAM_LIVE_TAG_KEY");
    let base = std::env::var("DAM_LIVE_TAG_BASE")
        .unwrap_or_else(|_| "https://api.deepseek.com/v1".into());
    let model =
        std::env::var("DAM_LIVE_TAG_MODEL").unwrap_or_else(|_| "deepseek-flash".into());
    // 默认直连（Some("")）：容器 env 代理常把 TLS 拦掉
    let proxy = std::env::var("DAM_LIVE_TAG_PROXY").ok().or(Some(String::new()));
    let cfg = TagSettings {
        enabled: true,
        provider: core_config::TAG_PROVIDER_DEEPSEEK.into(),
        base_url: base,
        api_key: key,
        model,
        max_tokens,
        timeout_ms: 120_000,
        proxy,
        ..Default::default()
    };
    let mut p = OpenAiCompatTagProvider::new(spec(), cfg);
    p.load().expect("load provider");
    p
}

struct Tally {
    ok: usize,
    repaired: usize,
    truncated_failed: usize,
    other_failed: usize,
    errors: Vec<String>,
    tag_counts: Vec<usize>,
}

fn run(p: &OpenAiCompatTagProvider, imgs: &[PathBuf]) -> Tally {
    let mut t = Tally {
        ok: 0,
        repaired: 0,
        truncated_failed: 0,
        other_failed: 0,
        errors: Vec::new(),
        tag_counts: Vec::new(),
    };
    for img in imgs {
        match p.tag_image(Path::new(img)) {
            Ok(out) => {
                t.ok += 1;
                if out.repaired {
                    t.repaired += 1;
                }
                t.tag_counts.push(out.tags.len());
                println!(
                    "  OK  {:<24} tags={} repaired={} title={}",
                    img.file_name().unwrap().to_string_lossy(),
                    out.tags.len(),
                    out.repaired,
                    out.title_ch
                );
            }
            Err(TagError::Truncated {
                max_tokens,
                budget,
                detail,
            }) => {
                t.truncated_failed += 1;
                let msg = format!(
                    "{}: truncated max_tokens={max_tokens}（{budget}）detail={}",
                    img.file_name().unwrap().to_string_lossy(),
                    detail.chars().take(160).collect::<String>()
                );
                println!("  TRUNC {msg}");
                t.errors.push(msg);
            }
            Err(e) => {
                t.other_failed += 1;
                let msg = format!("{}: {e}", img.file_name().unwrap().to_string_lossy());
                println!("  ERR  {}", msg.chars().take(200).collect::<String>());
                t.errors.push(msg);
            }
        }
    }
    t
}

#[test]
#[ignore = "需要真实 VL 端点 + Key（真机验收用）"]
fn live_deepseek_maxtokens_acceptance() {
    let limit: usize = std::env::var("DAM_LIVE_TAG_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    let imgs = corpus(limit);
    assert!(imgs.len() >= 20, "语料不足 20 张（卡要求 20+）: {}", imgs.len());
    println!("语料 {} 张：{:?}", imgs.len(), imgs.first());

    // ---- 口径 1：默认 2048（本次修复的默认值）→ 期望 0 失败 ----
    let p_default = provider(TagSettings::DEFAULT_MAX_TOKENS);
    println!("\n=== max_tokens={} (默认) ===", TagSettings::DEFAULT_MAX_TOKENS);
    let a = run(&p_default, &imgs);
    println!(
        "结果：ok={} repaired={} truncated={} other={} 平均标签数={:.1}",
        a.ok,
        a.repaired,
        a.truncated_failed,
        a.other_failed,
        if a.tag_counts.is_empty() {
            0.0
        } else {
            a.tag_counts.iter().sum::<usize>() as f64 / a.tag_counts.len() as f64
        }
    );
    assert_eq!(
        a.ok + a.truncated_failed + a.other_failed,
        imgs.len(),
        "每张图都要有明确结果"
    );
    assert_eq!(
        a.truncated_failed, 0,
        "默认 {} 下不应再有截断失败：{:?}",
        TagSettings::DEFAULT_MAX_TOKENS,
        a.errors
    );

    // ---- 口径 2：旧默认 1024（真机现场）→ 截断要么补齐、要么归为截断类失败 ----
    let p_old = provider(1024);
    println!("\n=== max_tokens=1024 (旧默认，真机现场) ===");
    let b = run(&p_old, &imgs);
    println!(
        "结果：ok={} repaired={} truncated={} other={}",
        b.ok, b.repaired, b.truncated_failed, b.other_failed
    );
    assert_eq!(
        b.other_failed, 0,
        "1024 下不允许出现非截断类失败（截断必须归类为 truncated，不能混成真错）：{:?}",
        b.errors
    );
    // 截断类失败必须是「补不出来」的那种（空 content：推理 token 把预算吃光），
    // 且错误文案带本次实际下发的 max_tokens=1024 —— 排障能直接看到预算。
    for e in &b.errors {
        assert!(
            e.contains("max_tokens=1024"),
            "截断类失败的文案必须带实际 max_tokens: {e}"
        );
        assert!(
            e.contains("finish_reason=length"),
            "截断类失败的文案必须带 finish_reason: {e}"
        );
    }
    // 真机现场 4/11 失败 → 修复后应看到「补齐」而不是失败（输出长度有波动，
    // 补齐次数按轮次记录；补齐路径本身由 mock 单测硬钉）
    if b.repaired == 0 {
        println!("注意：本轮 1024 未触发可补齐的截断（模型输出长度有波动），mock 单测兜底该路径");
    } else {
        println!("1024 下补齐救回 {}/{} 张（不再整张失败）", b.repaired, imgs.len());
    }
}

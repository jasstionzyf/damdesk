//! OCR 验收夹具生成：纯底图 + 多行文字（中/英/混合），供 ocr_bench / e2e 使用。
//!
//! 用法：cargo run --release -p engine-ocr --example ocr_fixture -- /tmp/ocr-corpus
//! 产出三张真实文字图（font = 包内 Noto Sans SC）：
//!   cn.png（中文两行）/ en.png（英文两行）/ mixed.png（中英混排三行）。
//!
//! 注意：core-media 的 libvips 解码缓存按「文件名+mtime」命中，同一路径秒内连续
//! 叠字会拿到旧解码（只见最后一行）。这里用「链式唯一文件名」逐行叠加绕开
//! （每步读一个全新文件名 → 缓存必 miss），生产水印插件同路径连打场景同样要避开。

use core_media::{encode_rgb_to, watermark_text, TextWatermark, WatermarkPosition};

/// stamp chain：src → dst，每次用新文件名。
fn stamp_chain(prev: &std::path::Path, next: &std::path::Path, text: &str, pos: WatermarkPosition, size: f32) {
    let wm = TextWatermark {
        text: text.to_string(),
        position: pos,
        opacity: 100,
        font_size: size,
        margin: 24,
        color: [10, 10, 10],
    };
    watermark_text(prev, next, &wm).expect("watermark");
}

fn make(dir: &std::path::Path, name: &str, w: i32, h: i32, lines: &[(&str, WatermarkPosition, f32)]) {
    let px = vec![245u8; (w * h * 3) as usize]; // 浅灰白底
    let blank = dir.join(format!("{name}.blank.png"));
    encode_rgb_to(w, h, &px, &blank, None).expect("encode blank");
    let mut prev: std::path::PathBuf = blank.clone();
    for (i, (text, pos, size)) in lines.iter().enumerate() {
        let next = dir.join(format!("{name}.step{i}.png"));
        stamp_chain(&prev, &next, text, *pos, *size);
        if prev != blank {
            let _ = std::fs::remove_file(&prev);
        }
        prev = next;
    }
    std::fs::rename(&prev, dir.join(name)).expect("rename final");
    let _ = std::fs::remove_file(&blank);
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/ocr-corpus"));
    std::fs::create_dir_all(&dir).expect("mkdir");

    make(&dir, "cn.png", 640, 320, &[
        ("桌面数字资产管理平台", WatermarkPosition::TopLeft, 40.0),
        ("全库文字识别验收样张", WatermarkPosition::BottomLeft, 36.0),
    ]);
    make(&dir, "en.png", 640, 320, &[
        ("Digital Asset Management 2026", WatermarkPosition::TopLeft, 36.0),
        ("OCR acceptance fixture 42", WatermarkPosition::BottomLeft, 32.0),
    ]);
    make(&dir, "mixed.png", 800, 400, &[
        ("SOUJPG DAM 图库平台", WatermarkPosition::TopLeft, 44.0),
        ("SKU-8891 降价 15%", WatermarkPosition::Center, 40.0),
        ("Model Release v6 medium", WatermarkPosition::BottomRight, 32.0),
    ]);
    // 大图（1920×1080，min 边 ≥ 736 → det 不放大，量 det 原尺寸开销）
    make(&dir, "big.png", 1920, 1080, &[
        ("批发零售行业周报 2026 年第 42 期", WatermarkPosition::TopLeft, 48.0),
        ("华东区销售额同比增长 23.5%", WatermarkPosition::TopRight, 44.0),
        ("重点 SKU：生鲜/日配/酒水", WatermarkPosition::Center, 44.0),
        ("数据来源：内部 BI 平台导出", WatermarkPosition::BottomLeft, 40.0),
        ("单位：万元（不含税）", WatermarkPosition::BottomRight, 40.0),
    ]);

    println!("fixtures written to {}", dir.display());
}

//! libvips 后端（Linux/macOS）。
//!
//! libvips 是进程全局状态（VipsInit 一次），用 OnceLock 守护初始化。

use std::path::Path;
use std::sync::OnceLock;

use libvips::ops::{self, Interpretation};
use libvips::VipsApp;

use crate::{fit_grid_32, MediaError, Result, PHASH_GRID};

fn ensure_init() -> Result<&'static VipsApp> {
    // 并发安全（D6 修复）：libvips 是进程全局单实例，两个线程同时 `VipsApp::default()`
    // 会触发 VIPS 内部断言崩溃（SIGABRT，t_xx 并发初始化）。用 get_or_init 保证只初始化一次。
    static APP: OnceLock<std::result::Result<VipsApp, ()>> = OnceLock::new();
    APP.get_or_init(|| {
        point_vips_libdir_at_bundle();
        VipsApp::default("soujpg-dam-desktop").map_err(|_| ())
    })
        .as_ref()
        .map_err(|_| MediaError::Init)
}

/// 自包含发行包（macOS .app，D13）：libvips 在**编译期 prefix/lib**（/opt/homebrew/
/// Cellar/vips/…）下找 `vips-modules-<ver>`——heif/jxl/magick/poppler/openslide 全是
/// 动态模块，装到没装 brew 的用户机器上该路径不存在，这些格式会**静默缺失**。
/// libvips 的 prefix 猜测读 `VIPSHOME` 环境变量（vips_guess_libdir(argv, "VIPSHOME")，
/// 实测 8.18.7：设了就完全接管，找 $VIPSHOME/lib/vips-modules-<ver>；VIPS_LIBDIR/
/// VIPS_PREFIX 均无效）：若 `.app/Contents/lib/vips-modules-*` 存在
/// （scripts/mac_bundle_dylibs.sh 放进去的）就把 VIPSHOME 指到 `.app/Contents`；
/// 开发态 / 测试容器没有该目录，什么都不做，行为与之前完全一致。
///
/// 只在 `ensure_init` 的 `get_or_init` 里调用（先于任何 `VipsApp` 初始化、全进程一次）；
/// 用户显式设置的 `VIPSHOME` 优先。
fn point_vips_libdir_at_bundle() {
    if std::env::var_os("VIPSHOME").is_some() {
        return;
    }
    let Some(exe) = std::env::current_exe().ok() else {
        return;
    };
    let Some(macos_dir) = exe.parent() else { return };
    // exe 在 .app/Contents/MacOS/，Contents 是 libvips 期望的 $prefix
    let Ok(contents) = macos_dir.join("..").canonicalize() else {
        return;
    };
    let has_modules = std::fs::read_dir(contents.join("lib")).ok().is_some_and(|rd| {
        rd.flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("vips-modules-"))
    });
    if has_modules {
        std::env::set_var("VIPSHOME", &contents);
    }
}

/// 显式初始化（进程级、幂等）：给**绕过 core-media 直接调 `libvips::ops::*`** 的调用方用。
///
/// 为什么必须有：libvips 未初始化时，多个线程同时首次触发内部 class 注册会撞车
/// （GLib `assertion 'parent_type > 0' failed` → 随后 `vips_black`/`vips_pngsave` 返回
/// 错误，表现为 `BlackError` / `PngsaveError`，严重时 SIGABRT）。集成测试入口先调本函数。
pub fn init() -> Result<()> {
    ensure_init().map(|_| ())
}

fn vips_err(app: &VipsApp) -> MediaError {
    app.error_buffer()
        .map(|s| MediaError::Vips(s.to_string()))
        .unwrap_or(MediaError::Vips("unknown vips error".into()))
}

/// 安全版「写内存」（原生像素裸字节）：语义等价 `VipsImage::image_write_to_memory()`
/// （= `write_to_buffer(".raw")`），但**带 vips 状态码**，失败是 `Err` 而不是进程 abort。
///
/// 为什么不用 libvips-rs 1.7.6 的 `image_write_to_memory()`：该 API 拿不到 vips 返回码，
/// 写失败时 `vips_image_write_to_memory` 返回 NULL，它仍直接
/// `slice::from_raw_parts(NULL, size)` —— debug 构建触发 std 的 unsafe precondition 检查，
/// **non-unwinding panic（整个进程 abort）**，调用方连 `Result` 都拿不到。
/// 实测（2026-10-16，EXIF2 验收）：语料里的损坏 JPEG（exif-samples/jpg/corrupted.jpg，
/// 中段数据损坏 → 解码报 `error in tile 0 x N`）走到 `luma32` 的写内存就把整个
/// app-web 进程 abort 了（导入 56/521 张即崩）。改走本函数后，同一张图变成
/// 「响亮 Err」（导入侧 `luma32(..).ok()` → pHash 置 NULL，其余流程照走）。
///
/// 为什么绕临时文件：容器内 libvips 是 8.14.1，`VipsForeignSaveRawBuffer`
/// （`vips_rawsave_buffer`）到 8.16 才有 —— 直接调 `ops::rawsave_buffer` 会在链接期
/// `undefined symbol: vips_rawsave_buffer`（已实测）。`ops::rawsave`（写文件）8.14 有，
/// 且返回 vips 状态码 → 失败为 Err；写盘的是**当前图像**（luma32 已是 ≤32×32 的灰度小图；
/// rgb8 是全分辨率，调用方是合规修复链路，非导入热点）。
fn write_raw(img: &libvips::VipsImage) -> Result<Vec<u8>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let app = ensure_init()?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "dam-raw-{}-{}-{}.raw",
        std::process::id(),
        seq,
        nanos
    ));
    let out = (|| -> Result<Vec<u8>> {
        ops::rawsave(img, &tmp.to_string_lossy()).map_err(|_| vips_err(app))?;
        Ok(std::fs::read(&tmp)?)
    })();
    let _ = std::fs::remove_file(&tmp);
    out
}

/// 图片像素尺寸（宽, 高）。
pub fn image_size(path: &Path) -> Result<(i32, i32)> {
    let app = ensure_init()?;
    let img =
        libvips::VipsImage::new_from_file(&path.to_string_lossy()).map_err(|_| vips_err(app))?;
    Ok((img.get_width(), img.get_height()))
}

/// 生成缩略图写盘：**长边 ≤ size 的等比内接，只缩不放**（`ops::thumbnail` 实测语义；
/// 目标宽高的统一口径见共享 `crate::thumb_dims` —— DESKTOP-THUMB-PORTRAIT-1 竖图语义
/// 双后端统一），不裁切，保持纵横比，格式 webp。
pub fn thumbnail_to(path: &Path, dest: &Path, size: i32) -> Result<()> {
    let app = ensure_init()?;
    let img = ops::thumbnail(&path.to_string_lossy(), size).map_err(|_| vips_err(app))?;
    ops::webpsave(&img, &dest.to_string_lossy()).map_err(|_| vips_err(app))?;
    Ok(())
}

/// VL 打标用的 JPEG（内存，不写盘）：**长边 = `target_long_side`**、等比不裁切、EXIF 自动旋转。
/// 目标尺寸由调用方算好（engine-tag 的「短边 512」规则），这里只负责缩 + 编码。
///
/// 质量走 libvips 的 `jpegsave_buffer` 默认档（q=75）。**不要**换 `jpegsave_buffer_with_opts`：
/// 本仓库的 libvips-rs 1.7.6 选项结构体比容器内 libvips 新，会报
/// `jpegsave_buffer: no property named 'keep'`（实测）。
///
/// 为什么在 core-media 做：有界缩放与格式归一都需要解码/重编码，vips 后端的 `thumbnail` 自带
/// EXIF 自动旋转，与缩略图路径同一套语义（Windows 侧由 `rustimg` 后端复刻，见其注释）。
pub fn vl_jpeg(path: &Path, target_long_side: i32) -> Result<Vec<u8>> {
    let app = ensure_init()?;
    // 实测（2026-10-15，容器内 libvips）：`ops::thumbnail(path, size)` 是**长边 ≤ size 的等比内接**
    // （3072² → 768²；3000×800 → 768×205；800×3000 → 205×768），不是「宽度约束」。
    // 所以这里把「目标长边」直接交给 vips，**不要**再按宽高比反算目标宽度（会把竖图缩两遍）。
    let img =
        ops::thumbnail(&path.to_string_lossy(), target_long_side).map_err(|_| vips_err(app))?;
    ops::jpegsave_buffer(&img).map_err(|_| vips_err(app))
}

/// 全分辨率 RGBA8 解码（N6b `/op/encode` 底座：编码要保留 alpha 通道）。
///
/// 契约（与 rustimg 后端逐条一致）：
/// - 返回 (宽, 高, 交错 RGBA，len = w*h*4)；
/// - 3 band → alpha 补 255；4 band → **原样保留**；1 band（灰度）→ 复制三通道 + alpha 255；
/// - 其余 band 数显式报错（响亮失败）；
/// - **不做** alpha 白底合成（与 `rgb8` 语义相反，`rgb8` 是检测器口径）；
/// - EXIF 方向不在此处旋转（由调用方用 `encode::orient_rgba` 摆正），与 `rgb8` 一致。
pub fn rgba8(path: &Path) -> Result<(i32, i32, Vec<u8>)> {
    let app = ensure_init()?;
    let img = libvips::VipsImage::new_from_file(&path.to_string_lossy()).map_err(|_| vips_err(app))?;
    let img = ops::colourspace(&img, Interpretation::Srgb).map_err(|_| vips_err(app))?;
    let (w, h) = (img.get_width(), img.get_height());
    let bands = img.get_bands();
    let n = (w as usize) * (h as usize);
    match bands {
        4 => {
            let buf = write_raw(&img)?;
            if buf.len() != n * 4 {
                return Err(MediaError::Vips(format!(
                    "rgba8: 缓冲 {} 字节预期，实际 {}（bands=4）",
                    n * 4,
                    buf.len()
                )));
            }
            Ok((w, h, buf))
        }
        3 => {
            let buf = write_raw(&img)?;
            if buf.len() != n * 3 {
                return Err(MediaError::Vips(format!(
                    "rgba8: 缓冲 {} 字节预期，实际 {}（bands=3）",
                    n * 3,
                    buf.len()
                )));
            }
            let mut out = Vec::with_capacity(n * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Ok((w, h, out))
        }
        1 => {
            let buf = write_raw(&img)?;
            if buf.len() < n {
                return Err(MediaError::Vips(format!(
                    "rgba8: 灰度缓冲过短 {}/{}",
                    buf.len(),
                    n
                )));
            }
            let mut out = Vec::with_capacity(n * 4);
            for &v in &buf[..n] {
                out.extend_from_slice(&[v, v, v, 255]);
            }
            Ok((w, h, out))
        }
        other => Err(MediaError::Vips(format!(
            "rgba8: 不支持的 band 数 {other}（预期 1/3/4）"
        ))),
    }
}

/// 全分辨率 RGB8 解码（C2 compliance classic 检测器像素档输入）。
///
/// 契约：返回 (宽, 高, 交错 RGB，len = w*h*3)。
/// - EXIF 方向**不**自动旋转（与 rustimg 后端行为一致 → 跨平台统计可比；边缘带类
///   统计与旋转无关）；需要方向感知的场景走 `vl_jpeg`（thumbnail 自带旋转）。
/// - alpha 通道合成到**白底**（商品图透明区按白底计，与白底判定语义一致）；
/// - 灰度源复制三通道；其余 band 数显式报错（响亮失败）。
pub fn rgb8(path: &Path) -> Result<(i32, i32, Vec<u8>)> {
    let app = ensure_init()?;
    let img = libvips::VipsImage::new_from_file(&path.to_string_lossy()).map_err(|_| vips_err(app))?;
    let img = ops::colourspace(&img, Interpretation::Srgb).map_err(|_| vips_err(app))?;
    let (w, h) = (img.get_width(), img.get_height());
    let bands = img.get_bands();
    let img = match bands {
        3 => img,
        4 => ops::flatten_with_opts(
            &img,
            &ops::FlattenOptions {
                background: vec![255.0, 255.0, 255.0],
                max_alpha: 255.0,
            },
        )
        .map_err(|_| vips_err(app))?,
        1 => img,
        other => {
            return Err(MediaError::Vips(format!(
                "rgb8: 不支持的 band 数 {other}（预期 1/3/4）"
            )))
        }
    };
    let buf = write_raw(&img)?;
    if bands == 1 {
        // 灰度 → 复制三通道
        let n = (w as usize) * (h as usize);
        if buf.len() < n {
            return Err(MediaError::Vips(format!(
                "rgb8: 灰度缓冲过短 {}/{}",
                buf.len(),
                n
            )));
        }
        let mut out = Vec::with_capacity(n * 3);
        for &v in &buf[..n] {
            out.extend_from_slice(&[v, v, v]);
        }
        return Ok((w, h, out));
    }
    let expect = (w as usize) * (h as usize) * 3;
    if buf.len() != expect {
        return Err(MediaError::Vips(format!(
            "rgb8: 缓冲 {expect} 字节预期，实际 {}（bands={bands}）",
            buf.len()
        )));
    }
    Ok((w, h, buf))
}

/// 等比缩小：**长边 ≤ max_side，只缩不放**（与 `vl_jpeg` 的 thumbnail 口径一致），
/// 按 dest 扩展名落盘（jpg/jpeg/png/webp，其余显式报错）。返回输出 (宽, 高)。
/// 只缩不放：无需缩小时同格式原样复制（不转码）；跨格式则仅转码不缩放。
/// 两后端同语义（不受旧 `thumbnail_to` 的宽度口径平台差异影响）。
pub fn resize_to(path: &Path, dest: &Path, max_side: i32) -> Result<(i32, i32)> {
    let app = ensure_init()?;
    let (ow, oh) = image_size(path)?;
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let src_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ow.max(oh) <= max_side {
        // 只缩不放：无缩放需求。同格式直接复制（保格式保画质）；跨格式仅转码不缩放。
        if ext == src_ext {
            std::fs::copy(path, dest)?;
            return Ok((ow, oh));
        }
    }
    let img = ops::thumbnail(&path.to_string_lossy(), max_side).map_err(|_| vips_err(app))?;
    let (w, h) = (img.get_width(), img.get_height());
    match ext.as_str() {
        "webp" => ops::webpsave(&img, &dest.to_string_lossy()).map_err(|_| vips_err(app))?,
        "jpg" | "jpeg" => {
            let bytes = ops::jpegsave_buffer(&img).map_err(|_| vips_err(app))?;
            std::fs::write(dest, bytes)?;
        }
        "png" => {
            let bytes = ops::pngsave_buffer(&img).map_err(|_| vips_err(app))?;
            std::fs::write(dest, bytes)?;
        }
        other => {
            return Err(MediaError::Vips(format!(
                "resize_to: 不支持的输出格式 .{other}"
            )))
        }
    }
    Ok((w, h))
}

/// 等比缩放（双向）：长边缩放到 long_side，可放大也可缩小（`resize` 精确倍率）；
/// 按 dest 扩展名落盘（jpg/jpeg/png/webp）。素材准备用（把短边抬到规则门槛之上）。
pub fn scale_to(path: &Path, dest: &Path, long_side: i32) -> Result<(i32, i32)> {
    let app = ensure_init()?;
    if long_side <= 0 {
        return Err(MediaError::Vips(format!("scale_to: 非法长边 {long_side}")));
    }
    let (ow, oh) = image_size(path)?;
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let scale = long_side as f64 / ow.max(oh).max(1) as f64;
    let img = libvips::VipsImage::new_from_file(&path.to_string_lossy()).map_err(|_| vips_err(app))?;
    let img = ops::resize(&img, scale).map_err(|_| vips_err(app))?;
    let (w, h) = (img.get_width(), img.get_height());
    match ext.as_str() {
        "webp" => ops::webpsave(&img, &dest.to_string_lossy()).map_err(|_| vips_err(app))?,
        "jpg" | "jpeg" => {
            let bytes = ops::jpegsave_buffer(&img).map_err(|_| vips_err(app))?;
            std::fs::write(dest, bytes)?;
        }
        "png" => {
            let bytes = ops::pngsave_buffer(&img).map_err(|_| vips_err(app))?;
            std::fs::write(dest, bytes)?;
        }
        other => {
            return Err(MediaError::Vips(format!(
                "scale_to: 不支持的输出格式 .{other}"
            )))
        }
    }
    Ok((w, h))
}

/// 裁剪矩形（x, y, w, h，像素坐标，越界即报错），按 dest 扩展名落盘
/// （jpg/jpeg/png/webp，其余显式报错）。返回输出 (宽, 高)。smart_crop 修复插件用（C3）。
/// EXIF 方向不自动旋转（与 rgb8/luma32 口径一致）；vips 后端 `extract_area` 像素精确。
pub fn crop_to(path: &Path, dest: &Path, x: i32, y: i32, w: i32, h: i32) -> Result<(i32, i32)> {
    let app = ensure_init()?;
    if x < 0 || y < 0 || w <= 0 || h <= 0 {
        return Err(MediaError::Vips(format!(
            "crop_to: 非法矩形 x={x} y={y} w={w} h={h}"
        )));
    }
    let img =
        libvips::VipsImage::new_from_file(&path.to_string_lossy()).map_err(|_| vips_err(app))?;
    let (ow, oh) = (img.get_width(), img.get_height());
    if x + w > ow || y + h > oh {
        return Err(MediaError::Vips(format!(
            "crop_to: 矩形 {x},{y} {w}x{h} 越界（源 {ow}x{oh}）"
        )));
    }
    let out = ops::extract_area(&img, x, y, w, h).map_err(|_| vips_err(app))?;
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "webp" => ops::webpsave(&out, &dest.to_string_lossy()).map_err(|_| vips_err(app))?,
        "jpg" | "jpeg" => {
            let bytes = ops::jpegsave_buffer(&out).map_err(|_| vips_err(app))?;
            std::fs::write(dest, bytes)?;
        }
        "png" => {
            let bytes = ops::pngsave_buffer(&out).map_err(|_| vips_err(app))?;
            std::fs::write(dest, bytes)?;
        }
        other => {
            return Err(MediaError::Vips(format!(
                "crop_to: 不支持的输出格式 .{other}"
            )))
        }
    }
    Ok((w, h))
}

/// 32x32 灰度网格（core-hash::phash 的输入）。
///
/// **契约：输出恒为 32×32 = 1024 字节**（D6 真图 e2e 修复）。
/// 旧实现直接把 libvips thumbnail 的原始像素返回，非方图会给出非 1024 长度
/// （600×400 → 32×21 = 672 字节；400×600 → 32×48 = 1536 字节）→ `core_hash::phash`
/// 一律 BadGrid，导入侧又静默吞掉错误，导致 resource.pHash 大面积 NULL（200 张真图里
/// 只有 26 张方形图成功）。现在：libvips 缩到短边 ≤32，再做等比「fit + 居中灰底补边」
/// 到 32×32；方形输入与旧行为逐字节一致。
pub fn luma32(path: &Path) -> Result<Vec<u8>> {
    let app = ensure_init()?;
    let img = ops::thumbnail(&path.to_string_lossy(), PHASH_GRID).map_err(|_| vips_err(app))?;
    let gray = ops::colourspace(&img, Interpretation::BW).map_err(|_| vips_err(app))?;
    let (w, h) = (gray.get_width(), gray.get_height());
    Ok(fit_grid_32(&write_raw(&gray)?, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_missing_file_errs_cleanly() {
        // 无头容器里 libvips 可用；缺文件应返回 Err 而非 panic
        let r = image_size(Path::new("/nonexistent/xx.jpg"));
        assert!(r.is_err());
    }

    /// 写一张最小 P6 PPM（宽高任意，不依赖 image crate）。
    fn write_ppm(path: &Path, w: i32, h: i32) {
        let mut buf = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h {
            for x in 0..w {
                let v = ((x * 255 / w.max(1)) as u8).wrapping_add((y as u8).wrapping_mul(7));
                buf.extend_from_slice(&[v, v / 2, 255u8.wrapping_sub(v)]);
            }
        }
        std::fs::write(path, buf).unwrap();
    }

    /// D6 回归：任意宽高都必须给出 32×32=1024 字节（旧实现非方图给 672/1536 →
    /// core-hash::phash 全部 BadGrid → resource.pHash 大面积 NULL）。
    #[test]
    fn luma32_is_always_32x32_grid() {
        let dir = std::env::temp_dir().join(format!("dam_luma32_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, w, h) in [
            ("landscape", 80, 40),
            ("portrait", 40, 80),
            ("square", 64, 64),
        ] {
            let p = dir.join(format!("{name}.ppm"));
            write_ppm(&p, w, h);
            let luma = luma32(&p).unwrap_or_else(|e| panic!("{name} luma32 失败: {e}"));
            assert_eq!(
                luma.len(),
                PHASH_GRID as usize * PHASH_GRID as usize,
                "{name} 网格应为 1024 字节"
            );
        }
    }
}

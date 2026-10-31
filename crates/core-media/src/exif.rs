//! EXIF2（task）— 元数据解析：`kamadak-exif` 纯 Rust 实现，无外部二进制
//! （ExifTool 被否：桌面分发多一个外部依赖不可接受）。
//!
//! 容器支持矩阵：
//! - JPEG / TIFF：`read_from_container` 直读（kamadak 原生）
//! - PNG：自走 chunk 扫描剥 `eXIf` chunk → `read_raw`
//! - WebP：RIFF 容器剥 `EXIF` chunk（含 4 字节透传头）→ `read_raw`
//! - HEIC/AVIF/GIF/BMP：显式 `ExifUnsupported`（调用方把原因写进 exif json，不静默）
//!
//! 判定口径（与验收 python 基线共享，全量写入 reports/EXIF2-metadata.md §判定标准）：
//! - takenAt = DateTimeOriginal 本地时间 − OffsetTimeOriginal 偏移（缺 OffsetTimeOriginal
//!   回落 OffsetTime，再缺按 UTC 视角）；DateTimeOriginal 缺失回落 IFD0 DateTime。
//!   恒存 unix 秒。原始字符串保留在 exif json（date_time_original / offset 字段）。
//! - ISO = ISOSpeedRatings，缺失回落 ISOSpeed
//! - aperture = FNumber 有理数 → f64（2 位小数）；focalLenMm = FocalLength 同
//! - shutter = ExposureTime：den==1 → "{n}s"；否则 "1/{d}"（1/3s 这类照原样写 "1/3"）
//! - Make / Model / LensModel：trim + 连续空白折叠为单空格；大小写保留（不做品牌大小写归一）
//! - orientation = EXIF Orientation 原值 1..8（>8 或 0 视为缺）
//! - GPS（隐私红线）：仅当 `want_gps=true` 才解析 GPS IFD；默认不读不入库不进 raw 摘要
//!
//! 依赖审计：kamadak-exif 0.6（BSD-2-Clause，允许链接进 AGPL-3.0 项目）。

use std::io::Read;
use std::path::Path;

use serde_json::json;

use crate::MediaError;

/// kamadak-exif 依赖版本（报告/审计用）。
pub const EXIF_PARSER: &str = "kamadak-exif 0.6 (BSD-2-Clause)";

/// EXIF 方向 5..=8 需要宽高互换（6/8 是 90° 旋转，5/7 带翻转）。
pub fn oriented_dims(w: i32, h: i32, orientation: Option<i64>) -> (i32, i32) {
    match orientation {
        Some(o) if (5..=8).contains(&o) => (h, w),
        _ => (w, h),
    }
}

/// 把 `image` crate 解码出的原始像素按 EXIF Orientation 摆正（rustimg 后端 +
/// 跨后端一致性用例共用；vips 后端的 thumbnail 自带同语义旋转）。
/// 与 vips `rgb8` 的「不旋转」契约无关——那些路径不走本函数。
pub fn apply_orientation(
    img: image::DynamicImage,
    orientation: u32,
) -> image::DynamicImage {
    use image::imageops as ops;
    match orientation {
        2 => ops::flip_horizontal(&img).into(),
        3 => ops::rotate180(&img).into(),
        4 => ops::flip_vertical(&img).into(),
        // 5 = transpose（左上↔右下对角翻转）= rotate90 + flip_horizontal
        5 => ops::flip_horizontal(&ops::rotate90(&img)).into(),
        6 => ops::rotate90(&img).into(),
        // 7 = transverse = rotate90 + flip_vertical
        7 => ops::flip_vertical(&ops::rotate90(&img)).into(),
        8 => ops::rotate270(&img).into(),
        _ => img,
    }
}

/// 解析结果摘要：派生列 + exif json 双轨的真值源。
#[derive(Debug, Clone, Default)]
pub struct ExifSummary {
    /// unix 秒（口径见模块注释）；无拍摄时间 → None（不许拿 mtime 顶替）
    pub taken_at: Option<i64>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub focal_mm: Option<f64>,
    pub aperture: Option<f64>,
    pub shutter: Option<String>,
    pub orientation: Option<i64>,
    /// GPS 仅在 want_gps=true 时出现
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    /// 关键字段原始展示串（真值 json，不做二次加工）
    pub raw: serde_json::Value,
}

/// 按扩展名解析一张图的 EXIF。
///
/// 返回 `Ok(None)` = 容器合法但**没有** EXIF 元数据（干净无标签）；
/// `Err` = 解析失败 / 容器不支持（错误文本可直接落 exif json 的 reason）。
pub fn parse_file(path: &Path, ext: &str, want_gps: bool) -> Result<Option<ExifSummary>, MediaError> {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    let blob: Option<Vec<u8>> = match e.as_str() {
        "jpg" | "jpeg" => jpeg_exif_blob(path)?,
        "tif" | "tiff" => {
            let mut data = Vec::new();
            std::fs::File::open(path)?.read_to_end(&mut data)?;
            Some(data)
        }
        "png" => png_exif_blob(path)?,
        "webp" => webp_exif_blob(path)?,
        other => {
            return Err(MediaError::ExifUnsupported(format!(
                "container .{other} 不支持 EXIF 提取（HEIC/AVIF 等见报告未覆盖清单）"
            )))
        }
    };
    match blob {
        None => Ok(None),
        Some(data) => {
            // 结构预检（EXIF2-ROBUST）：kamadak 在畸形偏移上会走 unsafe 切片构造，
            // 实测在 debug 构建下直接 abort 进程（Ubuntu 25/rustc 1.99 的 UB 断言），
            // 因此**我们自己先做边界校验**，畸形 blob 一律走 parse_failed 而非崩服务。
            validate_tiff(&data)?;
            match exif::Reader::new().read_raw(data) {
                Ok(exif) => Ok(Some(summary_from(&exif, want_gps))),
                Err(exif::Error::NotFound(_)) => Ok(None),
                Err(e) => Err(exif_err(e)),
            }
        }
    }
}

/// JPEG APP1(Exif) 段自剥（不做容器级信任；ELF 化的第二道防线在 validate_tiff）。
fn jpeg_exif_blob(path: &Path) -> Result<Option<Vec<u8>>, MediaError> {
    let mut f = std::fs::File::open(path)?;
    let mut sig = [0u8; 2];
    if f.read_exact(&mut sig).is_err() || sig != [0xFF, 0xD8] {
        return Err(MediaError::Exif("not a JPEG".into()));
    }
    loop {
        let mut m = [0u8; 2];
        if f.read_exact(&mut m).is_err() {
            return Ok(None);
        }
        if m[0] != 0xFF {
            // 扫描数据/失步：放弃（EXIF 必在 SOS 之前）
            return Ok(None);
        }
        let marker = m[1];
        if marker == 0xD9 || marker == 0xDA {
            return Ok(None); // EOI / SOS：之后不再有元数据段
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue; // 无长度段
        }
        let mut lb = [0u8; 2];
        f.read_exact(&mut lb)?;
        let len = u16::from_be_bytes(lb) as usize;
        if len < 2 {
            return Err(MediaError::Exif("JPEG 段长度非法".into()));
        }
        let mut body = vec![0u8; len - 2];
        f.read_exact(&mut body)?;
        if marker == 0xE1 && body.len() >= 6 && &body[0..6] == b"Exif\x00\x00" {
            return Ok(Some(body[6..].to_vec()));
        }
    }
}

/// TIFF/EXIF 结构边界预检：字节序 + 魔数 + 逐 IFD 逐 entry 的值区间检查
/// （size>4 的偏移必须落在 blob 内），子 IFD 指针递归（深度/访问上限防环）。
/// 只判「结构是否安全到能交给解析器」，不解释语义。
pub fn validate_tiff(blob: &[u8]) -> Result<(), MediaError> {
    if blob.len() < 8 {
        return Err(MediaError::Exif("TIFF 头不足 8 字节".into()));
    }
    let le = match &blob[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(MediaError::Exif("TIFF 字节序标记非法".into())),
    };
    let u16at = |b: &[u8], o: usize| -> Option<u16> {
        let s = b.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    };
    let u32at = |b: &[u8], o: usize| -> Option<u32> {
        let s = b.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([s[0], s[1], s[2], s[3]])
        } else {
            u32::from_be_bytes([s[0], s[1], s[2], s[3]])
        })
    };
    if u16at(blob, 2) != Some(42) {
        return Err(MediaError::Exif("TIFF 魔数 != 42".into()));
    }
    let mut visited: Vec<u32> = Vec::new();
    let mut queue: Vec<(u32, u8)> = vec![(
        u32at(blob, 4).ok_or_else(|| MediaError::Exif("IFD0 偏移缺失".into()))?,
        0,
    )];
    while let Some((off, depth)) = queue.pop() {
        if depth > 4 {
            return Err(MediaError::Exif("IFD 递归过深".into()));
        }
        if visited.contains(&off) {
            continue;
        }
        if visited.len() > 32 {
            return Err(MediaError::Exif("IFD 数量异常".into()));
        }
        visited.push(off);
        let base = off as usize;
        let count = u16at(blob, base)
            .ok_or_else(|| MediaError::Exif(format!("IFD 头越界 @{base}")))?
            as usize;
        if count > 512 {
            return Err(MediaError::Exif(format!("IFD 条目数异常 {count}")));
        }
        let end = base + 2 + count * 12 + 4;
        if end > blob.len() {
            return Err(MediaError::Exif(format!("IFD 区间越界 @{base}")));
        }
        for i in 0..count {
            let e = base + 2 + i * 12;
            let tag = u16at(blob, e).unwrap_or(0);
            let ty = u16at(blob, e + 2).unwrap_or(0);
            let cnt = u32at(blob, e + 4).unwrap_or(0);
            let unit = match ty {
                1 | 2 | 6 | 7 => 1u64,
                3 | 8 => 2,
                4 | 9 | 11 => 4,
                5 | 10 | 12 => 8,
                _ => 1, // 未知类型：按 1 字节保守估算
            };
            let size = unit.saturating_mul(cnt as u64);
            if size > 4 {
                let val_off = u32at(blob, e + 8).unwrap_or(u32::MAX) as u64;
                if val_off.saturating_add(size) > blob.len() as u64 {
                    return Err(MediaError::Exif(format!(
                        "字段值越界 tag=0x{tag:04x} off={val_off} size={size} blob={}",
                        blob.len()
                    )));
                }
            }
            // 子 IFD 指针：ExifIFD / GPSInfo / Interop
            if (tag == 0x8769 || tag == 0x8825 || tag == 0xA005) && ty == 4 && cnt == 1 {
                if let Some(p) = u32at(blob, e + 8) {
                    if (p as usize) < blob.len() {
                        queue.push((p, depth + 1));
                    }
                }
            }
        }
        // 下一 IFD（IFD1/缩略图）
        if let Some(next) = u32at(blob, base + 2 + count * 12) {
            if next != 0 && (next as usize) < blob.len() {
                queue.push((next, depth + 1));
            }
        }
    }
    Ok(())
}

/// 只取 Orientation（rustimg 后端摆正用，避免整份摘要的开销心理负担——实现同一次解析）。
pub fn orientation_of_file(path: &Path, ext: &str) -> Option<i64> {
    match parse_file(path, ext, false) {
        Ok(Some(s)) => s.orientation,
        _ => None,
    }
}

fn exif_err(e: exif::Error) -> MediaError {
    MediaError::Exif(e.to_string())
}

// ---------------------------------------------------------------- 摘要组装

fn short_text(f: &exif::Field) -> String {
    f.display_value().to_string().replace('\\', "")
}

fn summary_from(exif: &exif::Exif, want_gps: bool) -> ExifSummary {
    use exif::{In, Tag, Value};

    let text = |tag: Tag| -> Option<String> {
        exif.get_field(tag, In::PRIMARY).and_then(|f| match &f.value {
            Value::Ascii(v) => v.first().map(|b| {
                let s = String::from_utf8_lossy(b).trim_end_matches('\0').to_string();
                collapse_ws(&s)
            }),
            _ => Some(short_text(f)),
        })
    };

    let make = text(Tag::Make);
    let model = text(Tag::Model);
    let lens = text(Tag::LensModel).or_else(|| text(Tag::LensMake));

    let uint_of = |tag: Tag| -> Option<i64> {
        exif.get_field(tag, In::PRIMARY).and_then(|f| match &f.value {
            Value::Short(v) => v.first().map(|&x| x as i64),
            Value::Long(v) => v.first().map(|&x| x as i64),
            Value::Byte(v) => v.first().map(|&x| x as i64),
            // EXIF2 修复（2026-10-16 验收捕获）：**有真实相机/工具把 SHORT/LONG 系的标签
            // 写成有符号类型 8/9（SShort/SLong）** —— kamadak 会原样给出 SShort/SLong，
            // 漏掉这两支就会把「文件里明明有」的 Orientation/ISO 判成缺失。
            // 实测：exiftool 测试集 exv_FujiTags*.jpg 的 Orientation(0x0112) 存成 SLong=1 →
            // 旧实现落 NULL，验收比对 python 基线时被判不一致（22 条 orientation 差异里 10 条）。
            Value::SShort(v) => v.first().map(|&x| x as i64),
            Value::SLong(v) => v.first().map(|&x| x as i64),
            _ => None,
        })
    };
    let rational_of = |tag: Tag| -> Option<(u32, u32)> {
        exif.get_field(tag, In::PRIMARY).and_then(|f| match &f.value {
            Value::Rational(v) => v.first().map(|r| (r.num, r.denom)),
            Value::SRational(v) => v
                .first()
                .map(|r| (r.num.unsigned_abs(), r.denom.unsigned_abs())),
            _ => None,
        })
    };
    let ratio_f = |r: (u32, u32)| -> Option<f64> {
        if r.1 == 0 {
            None
        } else {
            Some((r.0 as f64 / r.1 as f64 * 100.0).round() / 100.0)
        }
    };

    let iso = uint_of(Tag::PhotographicSensitivity).or_else(|| uint_of(Tag::ISOSpeed));
    let focal_mm = rational_of(Tag::FocalLength).and_then(ratio_f);
    let aperture = rational_of(Tag::FNumber).and_then(ratio_f);
    let shutter = rational_of(Tag::ExposureTime).map(|(n, d)| shutter_text(n, d));
    let orientation = uint_of(Tag::Orientation).filter(|o| (1..=8).contains(o));

    // ---- 拍摄时间（口径见模块头注释） ----
    let dto_raw = text(Tag::DateTimeOriginal);
    let mod_raw = text(Tag::DateTime);
    let offset_raw = text(Tag::OffsetTimeOriginal)
        .or_else(|| text(Tag::OffsetTime));
    let (dt_source, dt_raw) = match (&dto_raw, &mod_raw) {
        (Some(s), _) => ("DateTimeOriginal", Some(s.clone())),
        (None, Some(s)) => ("DateTime", Some(s.clone())),
        _ => ("", None),
    };
    let taken_at = dt_raw.as_deref().and_then(parse_exif_datetime).map(|c| {
        let off = offset_raw.as_deref().and_then(parse_tz_offset).unwrap_or(0);
        civil_to_unix(c) - off
    });

    // ---- GPS（默认关闭：want_gps=false 时完全不读，raw 摘要也不带） ----
    let mut gps_lat = None;
    let mut gps_lon = None;
    if want_gps {
        let lat_ref = text(Tag::GPSLatitudeRef).map(|s| s.to_ascii_uppercase());
        let lon_ref = text(Tag::GPSLongitudeRef).map(|s| s.to_ascii_uppercase());
        let lat_dms: Option<Vec<(u32, u32)>> = exif
            .get_field(Tag::GPSLatitude, In::PRIMARY)
            .and_then(|f| match &f.value {
                Value::Rational(v) => Some(v.iter().map(|r| (r.num, r.denom)).collect()),
                _ => None,
            });
        let lon_dms: Option<Vec<(u32, u32)>> = exif
            .get_field(Tag::GPSLongitude, In::PRIMARY)
            .and_then(|f| match &f.value {
                Value::Rational(v) => Some(v.iter().map(|r| (r.num, r.denom)).collect()),
                _ => None,
            });
        if let (Some(dms), Some(r)) = (lat_dms, lat_ref) {
            gps_lat = Some(dms_to_deg(&dms) * if r.starts_with('S') { -1.0 } else { 1.0 });
        }
        if let (Some(dms), Some(r)) = (lon_dms, lon_ref) {
            gps_lon = Some(dms_to_deg(&dms) * if r.starts_with('W') { -1.0 } else { 1.0 });
        }
    }

    let mut raw = json!({
        "parser": EXIF_PARSER,
        "make": make,
        "model": model,
        "lens_model": lens,
        "orientation": orientation,
        "iso": iso,
        "f_number": aperture,
        "exposure_time": shutter,
        "focal_length_mm": focal_mm,
        "date_time_original": dto_raw,
        "offset_time_original": offset_raw,
        "date_time_source": dt_source,
    });
    if want_gps {
        raw["gps_latitude"] = json!(gps_lat);
        raw["gps_longitude"] = json!(gps_lon);
    }

    ExifSummary {
        taken_at,
        make,
        model,
        lens,
        iso,
        focal_mm,
        aperture,
        shutter,
        orientation,
        gps_lat,
        gps_lon,
        raw,
    }
}

/// 展示串口径： ExposureTime n/d → "1/d"（den==1 → "Ns"）。
fn shutter_text(n: u32, d: u32) -> String {
    if d == 0 {
        return format!("{n}/0");
    }
    if d == 1 {
        format!("{n}s")
    } else {
        format!("1/{}", round_ratio(d, n))
    }
}

/// "1/x" 的分母按可读性取整（1/250s 而不是 1/250.0）；分子非 1 时原样 n/d。
fn round_ratio(d: u32, n: u32) -> u32 {
    if n != 1 {
        return d;
    }
    // 1/256.3 → 256；1/0.9 之类不会出现（den>1 时恒 ≥1）
    d
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dms_to_deg(dms: &[(u32, u32)]) -> f64 {
    let part = |i: usize| -> f64 {
        let (n, d) = dms.get(i).copied().unwrap_or((0, 1));
        if d == 0 {
            0.0
        } else {
            n as f64 / d as f64
        }
    };
    let v = part(0) + part(1) / 60.0 + part(2) / 3600.0;
    (v * 1e7).round() / 1e7
}

/// "YYYY:MM:DD HH:MM:SS" → (y, m, d, hh, mm, ss)。宽松分割（某些机型逗号/点）。
fn parse_exif_datetime(s: &str) -> Option<(i64, i64, i64, i64, i64, i64)> {
    let nums: Vec<i64> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect();
    if nums.len() < 6 {
        return None;
    }
    let (y, mo, d, h, mi, sec) = (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || !(0..=23).contains(&h) || mi > 59 || sec > 60 {
        return None;
    }
    Some((y, mo, d, h, mi, sec))
}

/// "+08:00" / "-05:30" / "Z" → 秒偏移（东区为正）。
fn parse_tz_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("Z") || s == "+00:00" {
        return Some(0);
    }
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'+') => (1i64, &s[1..]),
        Some(b'-') => (-1i64, &s[1..]),
        _ => return None,
    };
    let mut it = rest.split(':');
    let h: i64 = it.next()?.trim().parse().ok()?;
    let m: i64 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
    Some(sign * (h * 3600 + m * 60))
}

/// civil → unix 秒（Howard Hinnant days_from_civil 算法，std-only）。
fn civil_to_unix(c: (i64, i64, i64, i64, i64, i64)) -> i64 {
    let (y, m, d, hh, mm, ss) = c;
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + hh * 3_600 + mm * 60 + ss
}

// ---------------------------------------------------------------- chunk 剥取

/// PNG eXIf chunk 剥取（只读扫描；无 eXIf → Ok(None)）。
fn png_exif_blob(path: &Path) -> Result<Option<Vec<u8>>, MediaError> {
    let mut f = std::fs::File::open(path)?;
    let mut sig = [0u8; 8];
    f.read_exact(&mut sig)?;
    if &sig != b"\x89PNG\r\n\x1a\n" {
        return Err(MediaError::Exif("PNG 签名不合法".into()));
    }
    // ImageMagick 把 EXIF 写成私有 tEXt 块（keyword `Raw profile type exif`，值是
    // 「\nexif\n<十进制长度>\n<十六进制>」文本）。PIL 读它 → 验收 python 基线拿得到值，
    // 我们原来判「容器干净无 EXIF」会把有元数据的图当无元数据（诚实性缺口）。
    // 这里与 eXIf 同口径解出；解码后同样过 validate_tiff 再交给 kamadak（安全边界不放宽）。
    let mut im_profile: Option<Vec<u8>> = None;
    loop {
        let mut lenb = [0u8; 4];
        if f.read_exact(&mut lenb).is_err() {
            return Ok(None); // 截断流：按无 eXIf 处理
        }
        let len = u32::from_be_bytes(lenb) as usize;
        if len > 64 * 1024 * 1024 {
            return Ok(None); // 畸形长度：不当容器读
        }
        let mut ctype = [0u8; 4];
        f.read_exact(&mut ctype)?;
        if &ctype == b"eXIf" {
            let mut data = vec![0u8; len];
            f.read_exact(&mut data)?;
            if data.is_empty() {
                return Ok(None);
            }
            return Ok(Some(data));
        }
        if &ctype == b"tEXt" && im_profile.is_none() {
            let mut data = vec![0u8; len];
            if f.read_exact(&mut data).is_ok() {
                let mut it = data.splitn(2, |b| *b == 0);
                let key = it.next().unwrap_or_default();
                let val = it.next().unwrap_or_default();
                if key == b"Raw profile type exif" {
                    im_profile = decode_im_raw_profile(val);
                }
            }
            let mut crc = [0u8; 4];
            if f.read_exact(&mut crc).is_err() {
                return Ok(None);
            }
            continue;
        }
        if &ctype == b"IEND" {
            return Ok(im_profile);
        }
        // 跳过 data + CRC
        if std::io::copy(&mut f.by_ref().take(len as u64), &mut std::io::sink()).is_err() {
            return Ok(None);
        }
        let mut crc = [0u8; 4];
        if f.read_exact(&mut crc).is_err() {
            return Ok(None);
        }
    }
}

/// 解 ImageMagick 私有 tEXt 块里的 `Raw profile type exif` 文本 → EXIF blob。
/// 文本形态：`\n<profile 名>\n<十进制字节数>\n<十六进制数据（按 72 字符折行）>`；
/// 解出的字节常带 `Exif\0\0` 头（与 JPEG APP1 内层一致）→ 剥掉后交给 TIFF 解析。
/// 只在解码结果过 `validate_tiff` 时才返回（畸形一律 None → 落 `no_exif`，不崩）。
fn decode_im_raw_profile(val: &[u8]) -> Option<Vec<u8>> {
    let s = String::from_utf8_lossy(val);
    let mut hex = String::new();
    let mut seen_len = false;
    for raw_line in s.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if !seen_len {
            // profile 名行（"exif"）+ 十进制长度行都不进数据
            if line.chars().all(|c| c.is_ascii_digit()) {
                seen_len = true;
            }
            continue;
        }
        if !line.chars().all(|c| c.is_ascii_hexdigit()) {
            return None; // 形态不符：不猜
        }
        hex.push_str(line);
    }
    if !seen_len || hex.len() < 16 || hex.len() % 2 != 0 {
        return None;
    }
    let b = hex.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for i in (0..b.len()).step_by(2) {
        let hi = (b[i] as char).to_digit(16)?;
        let lo = (b[i + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    if out.len() >= 6 && &out[0..6] == b"Exif\x00\x00" {
        out.drain(0..6);
    }
    validate_tiff(&out).ok()?;
    Some(out)
}

/// WebP RIFF EXIF chunk 剥取；EXIF payload 头 4 字节是透传偏移（通常全 0）→ 剥掉。
fn webp_exif_blob(path: &Path) -> Result<Option<Vec<u8>>, MediaError> {
    let mut data = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut data)?;
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return Err(MediaError::Exif("WebP/RIFF 头不合法".into()));
    }
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let fourcc = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        if body + size > data.len() {
            return Ok(None); // 截断
        }
        if fourcc == b"EXIF" {
            let mut payload = data[body..body + size].to_vec();
            // 透传头：前 4 字节为 TIFF 起始偏移，全 0 时直接剥
            if payload.len() >= 4 && payload[0..4] == [0, 0, 0, 0] {
                payload.drain(0..4);
            }
            // EXIF2 修复（2026-10-16 验收捕获）：真实 WebP（如 exiftool 测试集 / ImageMagick
            // 产出）的 EXIF chunk 里还带一层 `Exif\0\0` 头 → 变体是
            // `00 00 00 00 | Exif\0\0 | MM\0*…`。只剥 4 字节会把 `Exif\0\0` 留给 TIFF 解析器，
            // validate_tiff 直接判「字节序标记非法」→ 整批 webp 的 EXIF 全丢
            // （实测 webpexif_*.webp 7 张全 parse_failed，而 PIL 基线读得到）。
            if payload.len() >= 6 && &payload[0..6] == b"Exif\x00\x00" {
                payload.drain(0..6);
            }
            return Ok(Some(payload));
        }
        pos = body + size + (size & 1); // chunk 按偶数对齐
    }
    Ok(None)
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // ---- 测试专用 EXIF TIFF 构造器（不依赖 kamadak 写 API，字节级可控） ----

    #[derive(Clone, Copy, PartialEq)]
    enum Ifd {
        Main,
        Exif,
        Gps,
    }

    /// 测试专用 EXIF TIFF 构造器（不依赖 kamadak 写 API，字节级可控）。
    struct TiffBuilder {
        ifd0: Vec<[u8; 12]>,
        exif_ifd: Vec<[u8; 12]>,
        gps_ifd: Vec<[u8; 12]>,
        data: Vec<u8>,
    }

    const TYPE_ASCII: u16 = 2;
    const TYPE_SHORT: u16 = 3;
    const TYPE_LONG: u16 = 4;
    const TYPE_RATIONAL: u16 = 5;

    /// 单条 entry 组装：≤4B 内联，>4B 追加到值区（相对值区的偏移，emit 时加 data_off）。
    fn store_pair(
        ifd: &mut Vec<[u8; 12]>,
        data: &mut Vec<u8>,
        tag: u16,
        ty: u16,
        count: u32,
        bytes: Vec<u8>,
    ) {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&ty.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        if bytes.len() <= 4 {
            entry[8..8 + bytes.len()].copy_from_slice(&bytes);
        } else {
            let off = data.len() as u32;
            entry[8..12].copy_from_slice(&off.to_le_bytes());
            data.extend_from_slice(&bytes);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
        ifd.push(entry);
    }

    impl TiffBuilder {
        fn new() -> Self {
            Self {
                ifd0: vec![],
                exif_ifd: vec![],
                gps_ifd: vec![],
                data: vec![],
            }
        }

        fn ascii(&mut self, w: Ifd, tag: u16, s: &str) {
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            let (ifd, data) = match w {
                Ifd::Main => (&mut self.ifd0, &mut self.data),
                Ifd::Exif => (&mut self.exif_ifd, &mut self.data),
                Ifd::Gps => (&mut self.gps_ifd, &mut self.data),
            };
            store_pair(ifd, data, tag, TYPE_ASCII, bytes.len() as u32, bytes);
        }

        fn short(&mut self, w: Ifd, tag: u16, v: u16) {
            let (ifd, data) = match w {
                Ifd::Main => (&mut self.ifd0, &mut self.data),
                Ifd::Exif => (&mut self.exif_ifd, &mut self.data),
                Ifd::Gps => (&mut self.gps_ifd, &mut self.data),
            };
            store_pair(
                ifd,
                data,
                tag,
                TYPE_SHORT,
                1,
                v.to_le_bytes().to_vec(),
            );
        }

        fn rational(&mut self, w: Ifd, tag: u16, n: u32, d: u32) {
            self.rationals(w, tag, &[(n, d)]);
        }

        fn rationals(&mut self, w: Ifd, tag: u16, vals: &[(u32, u32)]) {
            let mut b = Vec::new();
            for (n, d) in vals {
                b.extend_from_slice(&n.to_le_bytes());
                b.extend_from_slice(&d.to_le_bytes());
            }
            let (ifd, data) = match w {
                Ifd::Main => (&mut self.ifd0, &mut self.data),
                Ifd::Exif => (&mut self.exif_ifd, &mut self.data),
                Ifd::Gps => (&mut self.gps_ifd, &mut self.data),
            };
            store_pair(ifd, data, tag, TYPE_RATIONAL, vals.len() as u32, b);
        }

        /// 组装完整 TIFF blob：IFD0 →（ExifIFD / GPSIFD 指针）→ 值区。
        fn build(mut self) -> Vec<u8> {
            let by_tag = |e: &[u8; 12]| u16::from_le_bytes([e[0], e[1]]);
            self.ifd0.sort_by_key(|e| by_tag(e));
            self.exif_ifd.sort_by_key(|e| by_tag(e));
            self.gps_ifd.sort_by_key(|e| by_tag(e));

            // 先加 IFD 指针条目（inline LONG），再算布局——尺寸必须含指针条目
            let exif_ptr = !self.exif_ifd.is_empty();
            let gps_ptr = !self.gps_ifd.is_empty();
            if exif_ptr {
                self.ifd0.push([0; 12]);
            }
            if gps_ptr {
                self.ifd0.push([0; 12]);
            }
            self.ifd0.sort_by_key(|e| u16::from_le_bytes([e[0], e[1]]));

            // 头 8B + IFD0 + ExifIFD + GPSIFD + 值区
            let mut out = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
            let ifd0_off = out.len() as u32;
            let ifd0_size = 2 + 12 * self.ifd0.len() as u32 + 4;
            let exif_off = ifd0_off + ifd0_size;
            let exif_size = if exif_ptr { 2 + 12 * self.exif_ifd.len() as u32 + 4 } else { 0 };
            let gps_off = exif_off + exif_size;
            let gps_size = if gps_ptr { 2 + 12 * self.gps_ifd.len() as u32 + 4 } else { 0 };
            let data_off = gps_off + gps_size;

            let patch = |e: &mut [u8; 12], tag: u16, off: u32| {
                e[0..2].copy_from_slice(&tag.to_le_bytes());
                e[2..4].copy_from_slice(&TYPE_LONG.to_le_bytes());
                e[4..8].copy_from_slice(&1u32.to_le_bytes());
                e[8..12].copy_from_slice(&off.to_le_bytes());
            };
            let mut it = self.ifd0.iter_mut().filter(|e| *e == &[0u8; 12]);
            if exif_ptr {
                if let Some(slot) = it.next() {
                    patch(slot, 0x8769, exif_off);
                }
            }
            if gps_ptr {
                if let Some(slot) = it.next() {
                    patch(slot, 0x8825, gps_off);
                }
            }

            let mut emit = |out: &mut Vec<u8>, ifd: &Vec<[u8; 12]>, next: u32| {
                out.extend_from_slice(&(ifd.len() as u16).to_le_bytes());
                for mut e in ifd.clone() {
                    let cnt = u32::from_le_bytes(e[4..8].try_into().unwrap());
                    let ty = u16::from_le_bytes(e[2..4].try_into().unwrap());
                    let bytes_len = match ty {
                        TYPE_ASCII => cnt,
                        TYPE_LONG => cnt * 4,
                        TYPE_RATIONAL => cnt * 8,
                        TYPE_SHORT => cnt * 2,
                        _ => cnt * 4,
                    };
                    if bytes_len > 4 {
                        // 值区偏移：相对 → 绝对（IFD 指针类 inline 条目 bytes_len==4，不走这里）
                        let off = u32::from_le_bytes(e[8..12].try_into().unwrap());
                        e[8..12].copy_from_slice(&(data_off + off).to_le_bytes());
                    }
                    out.extend_from_slice(&e);
                }
                out.extend_from_slice(&next.to_le_bytes());
            };

            emit(&mut out, &self.ifd0, 0);
            if !self.exif_ifd.is_empty() {
                emit(&mut out, &self.exif_ifd, 0);
            }
            if !self.gps_ifd.is_empty() {
                emit(&mut out, &self.gps_ifd, 0);
            }
            out.extend_from_slice(&self.data);
            out
        }
    }

    /// 带元数据的最小 JPEG（image crate 编码 + SOI 后插 APP1）。
    pub(crate) fn jpeg_with_exif(tiff: &[u8], w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h);
        let mut base = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut base),
            image::ImageFormat::Jpeg,
        )
        .expect("encode base jpeg");
        let mut out = base[..2].to_vec(); // SOI
        let app1_len = (2 + 6 + tiff.len()) as u16; // len 字段含自身
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&app1_len.to_be_bytes());
        out.extend_from_slice(b"Exif\x00\x00");
        out.extend_from_slice(tiff);
        out.extend_from_slice(&base[2..]);
        out
    }

    fn tmpfile(name: &str, data: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dam_exif_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        p
    }

    pub(crate) fn phone_tiff() -> Vec<u8> {
        let mut t = TiffBuilder::new();
        t.ascii(Ifd::Main, 0x010F, "Apple");
        t.ascii(Ifd::Main, 0x0110, "iPhone 14 Pro");
        t.short(Ifd::Main, 0x0112, 6); // Orientation = 6（竖拍）
        t.ascii(Ifd::Exif, 0x9003, "2024:05:01 10:20:30");
        t.ascii(Ifd::Exif, 0x9010, "+08:00");
        t.short(Ifd::Exif, 0x8827, 1250);
        t.rational(Ifd::Exif, 0x829A, 1, 250); // ExposureTime 1/250
        t.rational(Ifd::Exif, 0x829D, 56, 10); // FNumber 5.6
        t.rational(Ifd::Exif, 0x920A, 24, 1); // FocalLength 24mm
        t.ascii(Ifd::Exif, 0xA434, "iPhone 14 Pro back triple camera 6.86mm f/1.78");
        t.build()
    }

    #[test]
    fn parses_phone_jpeg_core_fields() {
        let p = tmpfile("phone.jpg", &jpeg_with_exif(&phone_tiff(), 4032, 3024));
        let s = parse_file(&p, "jpg", false).unwrap().expect("has exif");
        assert_eq!(s.make.as_deref(), Some("Apple"));
        assert_eq!(s.model.as_deref(), Some("iPhone 14 Pro"));
        assert_eq!(s.orientation, Some(6));
        assert_eq!(s.iso, Some(1250));
        assert_eq!(s.shutter.as_deref(), Some("1/250"));
        assert_eq!(s.aperture, Some(5.6));
        assert_eq!(s.focal_mm, Some(24.0));
        // 2024-05-01 10:20:30 +08:00 → unix 秒
        let want = civil_to_unix((2024, 5, 1, 10, 20, 30)) - 8 * 3600;
        assert_eq!(s.taken_at, Some(want));
        // want_gps=false → GPS 字段不进摘要（隐私红线）
        assert!(s.gps_lat.is_none());
        assert!(s.raw.get("gps_latitude").is_none());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn no_exif_jpeg_returns_none() {
        // image crate 默认不写 EXIF
        let img = image::DynamicImage::new_rgb8(8, 8);
        let mut base = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut base), image::ImageFormat::Jpeg)
            .unwrap();
        let p = tmpfile("noexif.jpg", &base);
        assert!(parse_file(&p, "jpg", false).unwrap().is_none());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn gps_parsing_and_privacy_gate() {
        let mut t = TiffBuilder::new();
        t.ascii(Ifd::Main, 0x010F, "TestCam");
        t.ascii(Ifd::Gps, 0x0001, "N");
        t.rationals(Ifd::Gps, 0x0002, &[(22, 1), (32, 1), (3600, 100)]);
        t.ascii(Ifd::Gps, 0x0003, "E");
        t.rationals(Ifd::Gps, 0x0004, &[(114, 1), (3, 1), (7200, 100)]);
        let tiff = t.build();
        let p = tmpfile("gps.jpg", &jpeg_with_exif(&tiff, 16, 16));

        // 默认（want_gps=false）：坐标不解析不落任何字段
        let s = parse_file(&p, "jpg", false).unwrap().unwrap();
        assert!(s.gps_lat.is_none() && s.gps_lon.is_none());
        assert!(s.raw.get("gps_latitude").is_none());

        // 显式开启：22°32′36″N 114°03′72″E
        let s = parse_file(&p, "jpg", true).unwrap().unwrap();
        assert!((s.gps_lat.unwrap() - 22.543333).abs() < 1e-5, "{:?}", s.gps_lat);
        assert!((s.gps_lon.unwrap() - 114.07).abs() < 1e-5, "{:?}", s.gps_lon);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn png_exif_chunk_parsed() {
        // PNG 基图 + 注入 eXIf chunk
        let img = image::DynamicImage::new_rgb8(8, 8);
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let tiff = phone_tiff();
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&(tiff.len() as u32).to_be_bytes());
        chunk.extend_from_slice(b"eXIf");
        chunk.extend_from_slice(&tiff);
        chunk.extend_from_slice(&crc32(b"eXIf", &tiff).to_be_bytes());
        // 插到 IEND 前
        let iend = png.len() - 12;
        let mut out = Vec::new();
        out.extend_from_slice(&png[..iend]);
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&png[iend..]);
        let p = tmpfile("exif.png", &out);
        let s = parse_file(&p, "png", false).unwrap().expect("png exif");
        assert_eq!(s.model.as_deref(), Some("iPhone 14 Pro"));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn webp_exif_chunk_parsed() {
        let img = image::DynamicImage::new_rgb8(8, 8);
        let mut webp = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut webp), image::ImageFormat::WebP)
            .unwrap();
        let tiff = phone_tiff();
        // RIFF 尾插 EXIF chunk（偶对齐）
        let mut body = webp[12..].to_vec();
        if body.len() % 2 == 1 {
            body.push(0);
        }
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"EXIF");
        chunk.extend_from_slice(&((tiff.len() + 4) as u32).to_le_bytes());
        chunk.extend_from_slice(&0u32.to_le_bytes()); // 透传头
        chunk.extend_from_slice(&tiff);
        if tiff.len() % 2 == 1 {
            chunk.push(0);
        }
        body.extend_from_slice(&chunk);
        let riff_size = (4 + body.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&riff_size.to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&body);
        let p = tmpfile("exif.webp", &out);
        let s = parse_file(&p, "webp", false).unwrap().expect("webp exif");
        assert_eq!(s.orientation, Some(6));
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn unsupported_container_is_loud() {
        let p = tmpfile("x.heic", b"invalid");
        let err = parse_file(&p, "heic", false).unwrap_err();
        assert!(err.to_string().contains("heic"), "{err}");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn orientation_dims_and_apply() {
        assert_eq!(oriented_dims(4000, 3000, Some(6)), (3000, 4000));
        assert_eq!(oriented_dims(4000, 3000, Some(1)), (4000, 3000));
        assert_eq!(oriented_dims(4000, 3000, None), (4000, 3000));
        // apply_orientation：6 → 90° CW（4000×3000 → 3000×4000）
        let img = image::DynamicImage::new_rgb8(4, 2);
        let rot = apply_orientation(img, 6);
        assert_eq!((rot.width(), rot.height()), (2, 4));
    }

    #[test]
    fn timezone_and_datetime_edge_cases() {
        // 无 OffsetTime → 按 UTC
        let mut t = TiffBuilder::new();
        t.ascii(Ifd::Exif, 0x9003, "2020:01:02 03:04:05");
        let p = tmpfile("notz.jpg", &jpeg_with_exif(&t.build(), 4, 4));
        let s = parse_file(&p, "jpg", false).unwrap().unwrap();
        assert_eq!(s.taken_at, Some(civil_to_unix((2020, 1, 2, 3, 4, 5))));
        std::fs::remove_file(&p).unwrap();

        // 非法日期 → takenAt None（不崩不编造）
        let mut t = TiffBuilder::new();
        t.ascii(Ifd::Exif, 0x9003, "0000:00:00 00:00:00");
        let p = tmpfile("baddate.jpg", &jpeg_with_exif(&t.build(), 4, 4));
        let s = parse_file(&p, "jpg", false).unwrap().unwrap();
        assert_eq!(s.taken_at, None);
        std::fs::remove_file(&p).unwrap();
    }



    #[test]
    fn validate_tiff_rejects_out_of_range_values() {
        // 正常 blob：IFD0 两条 ASCII（值 > 4B → 走值偏移，才有可改的偏移字段）
        // ⚠️ 别用 `TiffBuilder::new().build()`：空 IFD0 只有 14 字节（8 头 + 2 计数 + 4 next），
        // 下面按「第 2 条 entry 的值偏移」定位会直接切片越界 panic（2026-10-16 全量测试捕获）。
        let t = {
            let mut b = TiffBuilder::new();
            b.ascii(Ifd::Main, 0x010F, "Canon");
            b.ascii(Ifd::Main, 0x0110, "EOS 5D");
            b.build()
        };
        validate_tiff(&t).expect("正常 TIFF 应通过预检");
        // 把第 2 条 entry 的值偏移改成越界（模拟 exif-samples 的 corrupted.jpg 类畸形）
        let mut bad = t.clone();
        bad[8 + 2 + 12 + 8..8 + 2 + 12 + 12].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        assert!(validate_tiff(&bad).is_err(), "越界偏移必须被预检拦下");
        // 魔数错 / 头截断
        assert!(validate_tiff(b"II\x2a\x00\x08\x00").is_err());
        let mut magic_bad = t.clone();
        magic_bad[2] = 43;
        assert!(validate_tiff(&magic_bad).is_err());
    }

    #[test]
    fn corrupt_exif_in_jpeg_is_reported_not_crashing() {
        let mut t = phone_tiff();
        // 定点破坏值偏移
        t[8 + 2 + 12 + 8..8 + 2 + 12 + 12].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        let p = tmpfile("corrupt.jpg", &jpeg_with_exif(&t, 64, 48));
        let r = parse_file(&p, "jpg", false);
        assert!(r.is_err(), "畸形 EXIF 必须走 Err（落 parse_failed），不得 panic/abort");
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn strip_gps_jpeg_removes_gps_keeps_rest() {
        let mut t = TiffBuilder::new();
        t.ascii(Ifd::Main, 0x010F, "Apple");
        t.short(Ifd::Main, 0x0112, 6);
        t.ascii(Ifd::Exif, 0x9003, "2024:05:01 10:20:30");
        t.ascii(Ifd::Gps, 0x0001, "N");
        t.rationals(Ifd::Gps, 0x0002, &[(22, 1), (30, 1), (0, 1)]);
        t.ascii(Ifd::Gps, 0x0003, "E");
        t.rationals(Ifd::Gps, 0x0004, &[(114, 1), (3, 1), (0, 1)]);
        let j = jpeg_with_exif(&t.build(), 16, 16);
        let p = tmpfile("gps2.jpg", &j);

        let stripped = strip_gps_jpeg(&j).expect("strip ok");
        assert!(stripped.len() < j.len() + 8);
        let ps = tmpfile("gps2_stripped.jpg", &stripped);
        // 剥离后：GPS 读不到（want_gps=true 也不出坐标），其余字段保留
        let s = parse_file(&ps, "jpg", true).unwrap().unwrap();
        assert!(s.gps_lat.is_none() && s.gps_lon.is_none());
        assert_eq!(s.model.as_deref(), None); // 该用例没写 Model
        assert_eq!(s.make.as_deref(), Some("Apple"));
        assert_eq!(s.taken_at.is_some(), true);
        std::fs::remove_file(&p).unwrap();
        std::fs::remove_file(&ps).unwrap();
    }

    #[test]
    fn strip_gps_jpeg_noop_when_clean() {
        let mut t = TiffBuilder::new();
        t.ascii(Ifd::Main, 0x010F, "NoGps");
        let j = jpeg_with_exif(&t.build(), 8, 8);
        let stripped = strip_gps_jpeg(&j).unwrap();
        assert_eq!(stripped, j, "无 GPS 必须逐字节原样（不重编码）");
    }

    #[test]
    fn shutter_variants() {
        assert_eq!(shutter_text(1, 250), "1/250");
        assert_eq!(shutter_text(30, 1), "30s");
        assert_eq!(shutter_text(1, 2), "1/2");
    }

    #[test]
    fn tz_parse() {
        assert_eq!(parse_tz_offset("+08:00"), Some(28_800));
        assert_eq!(parse_tz_offset("-05:30"), Some(-19_800));
        assert_eq!(parse_tz_offset("Z"), Some(0));
        assert_eq!(parse_tz_offset("bogus"), None);
    }

    // 纯测试 CRC32（PNG chunk 用；查表法 8 行）
    fn crc32(kind: &[u8], data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (i, e) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        let mut crc = 0xFFFF_FFFFu32;
        for b in kind.iter().chain(data.iter()) {
            crc = table[((crc ^ *b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    #[test]
    fn dump_tiff_hex() {
        let t = phone_tiff();
        let hex: Vec<String> = t.iter().map(|b| format!("{b:02x}")).collect();
        println!("HEXDUMP len={} {}", t.len(), hex.join(" "));
    }
}


/// GPS 剥离（导出链路）：JPEG 重排 APP1（去掉 GPS IFD 全部字段 + IFD0 的 GPSInfo
/// 指针），其余 EXIF 原样保留。无 EXIF / 无 GPS 的图原样返回。
/// PNG/WebP 的整 chunk 剥离 v1 未做（见报告未覆盖清单）。
pub fn strip_gps_jpeg(jpeg: &[u8]) -> Result<Vec<u8>, MediaError> {
    let mut rdr = std::io::Cursor::new(jpeg);
    let tiff = exif::Reader::new()
        .read_from_container(&mut rdr)
        .map_err(exif_err)?;
    let has_gps = tiff.fields().any(|f| is_gps_tag(f.tag.number()));
    if !has_gps {
        return Ok(jpeg.to_vec());
    }
    let little = tiff.little_endian();
    // 重建 TIFF：过滤 GPS 段字段（In::PRIMARY 且 tag < 0x20）+ IFD0 GPSInfo 指针
    let kept: Vec<exif::Field> = tiff
        .fields()
        .filter(|f| {
            !is_gps_tag(f.tag.number())
                && !(f.ifd_num == exif::In::PRIMARY && f.tag.number() == 0x8825)
        })
        .cloned()
        .collect();
    let out: Vec<u8>;
    {
        let mut buf: Vec<u8> = Vec::new();
        let mut writer = exif::experimental::Writer::new();
        for f in &kept {
            writer.push_field(f);
        }
        writer.write(&mut std::io::Cursor::new(&mut buf), little).map_err(exif_err)?;
        out = buf;
    }
    rebuild_jpeg_app1(jpeg, &out)
}

fn is_gps_tag(code: u16) -> bool {
    // GPS IFD 字段 tag 恒在 0x0000..=0x001F（kamadak 把它们挂 In::PRIMARY）
    code <= 0x001F
}

/// JPEG 段级重排：替换（或删除空的）Exif APP1；其余段字节原样。
fn rebuild_jpeg_app1(jpeg: &[u8], new_tiff: &[u8]) -> Result<Vec<u8>, MediaError> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return Err(MediaError::Exif("not a JPEG".into()));
    }
    let mut out = vec![0xFF, 0xD8];
    let mut i = 2usize;
    let mut inserted = false;
    while i + 4 <= jpeg.len() && jpeg[i] == 0xFF {
        let marker = jpeg[i + 1];
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&jpeg[i..i + 2]);
            i += 2;
            continue;
        }
        if marker == 0xD9 {
            out.extend_from_slice(&jpeg[i..]);
            i = jpeg.len();
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        let seg_end = i + 2 + len;
        let is_exif_app1 = marker == 0xE1 && jpeg.len() >= i + 10 && &jpeg[i + 4..i + 10] == b"Exif\x00\x00";
        if is_exif_app1 {
            if !inserted && !new_tiff.is_empty() {
                push_app1(&mut out, new_tiff);
                inserted = true;
            }
            i = seg_end;
            continue;
        }
        // SOS 之后的扫描数据整块拷贝
        if marker == 0xDA {
            out.extend_from_slice(&jpeg[i..]);
            return Ok(out);
        }
        out.extend_from_slice(&jpeg[i..seg_end]);
        i = seg_end;
    }
    if !inserted && !new_tiff.is_empty() {
        // 原图没有 APP1 却有 GPS？不可能；防御性插入
        let _ = out.split_off(2); // 不应走到这里；保守恢复 SOI+新 APP1
        let mut out2 = vec![0xFF, 0xD8];
        push_app1(&mut out2, new_tiff);
        out2.extend_from_slice(&jpeg[2..]);
        return Ok(out2);
    }
    Ok(out)
}

fn push_app1(out: &mut Vec<u8>, tiff: &[u8]) {
    let len = (2 + 6 + tiff.len()) as u16;
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(b"Exif\x00\x00");
    out.extend_from_slice(tiff);
}

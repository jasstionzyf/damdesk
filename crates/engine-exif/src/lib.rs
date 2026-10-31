//! L1 `engine-exif` — EXIF2（task）元数据落库底座。
//!
//! 为什么独立成 crate：解析落库这 40 行胶水被两条 L2 链共用——导入路径
//! （svc-import）与回填队列（svc-pipeline）；svc-* 同层禁互引（分层铁律），
//! core-media(L0) 又不能背网络依赖（逆地理编码），所以落 L1。
//!
//! 职责边界：
//! - core-media::exif：纯解析（kamadak-exif，无副作用）
//! - 这里：投影到 core-storage::exif_meta 派生列 + GPS 档位闸门 + 逆编码 provider
//! - 调用方：svc-import（导入时）、svc-pipeline::exif_queue（存量回填）
//!
//! 隐私红线（验收 #7，写进报告）：GPS 只在 `ExifSettings.gps != Off` 时解析；
//! Off 档连 GPS IFD 都不读；Place 档坐标剥除不落库，只留地名文本；
//! 坐标不进任何日志/telemetry。

use std::path::Path;

use core_config::{ExifSettings, GpsMode};
use core_storage::exif_meta::{self, ExifColumns};
use rusqlite::Connection;
use serde_json::json;

/// 单张图的 EXIF 应用结果（导入路径不因元数据失败而失败——原因写进 exif json）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExifApplyOutcome {
    /// 解析成功，派生列已写
    Parsed,
    /// 无 EXIF / 解析失败 / 格式不支持，原因已写 exif json（reason）
    Absent(String),
    /// 设置关闭（parse_on_import=false）：完全未动，exif 保持 NULL（留给 backfill）
    SkippedDisabled,
}

/// 导入/回填共用的落库入口：解析 → GPS 闸门 → 投影 → 全量覆盖写（幂等）。
pub fn apply_for_asset(
    conn: &Connection,
    asset_id: i64,
    path: &Path,
    ext: &str,
    cfg: &ExifSettings,
) -> Result<ExifApplyOutcome, String> {
    if !cfg.parse_on_import {
        return Ok(ExifApplyOutcome::SkippedDisabled);
    }
    match core_media::parse_file(path, ext, cfg.want_gps()) {
        Ok(Some(summary)) => {
            let mut cols = project(&summary);
            // GPS 档位闸门（第二道防线；解析层已经不读，这里保证不落库）
            match cfg.gps {
                GpsMode::Off => {
                    cols.gps_lat = None;
                    cols.gps_lon = None;
                    cols.gps_source = None;
                }
                GpsMode::Coords => {
                    if cols.gps_lat.is_some() || cols.gps_lon.is_some() {
                        cols.gps_source = Some("exif".into());
                    }
                }
                GpsMode::Place => {
                    // Place 档：坐标剥除不落库；地名（逆编码成功才有）进 exif json
                    let lat = cols.gps_lat.take();
                    let lon = cols.gps_lon.take();
                    cols.gps_source = None;
                    if let (Some(lat), Some(lon)) = (lat, lon) {
                        match reverse_geocode(&cfg.gps_provider.name, &cfg.gps_provider.base_url, lat, lon) {
                            Ok(place) => {
                                let mut raw = summary.raw.clone();
                                raw["placeName"] = json!(place);
                                raw["placeProvider"] = json!(cfg.gps_provider.name);
                                cols.exif_json = raw.to_string();
                                cols.gps_source = Some(format!("exif_place:{}", cfg.gps_provider.name));
                            }
                            Err(e) => {
                                let mut raw = summary.raw.clone();
                                raw["placeReason"] = json!(e);
                                cols.exif_json = raw.to_string();
                            }
                        }
                    }
                }
            }
            exif_meta::apply_columns(conn, asset_id, &cols).map_err(|e| e.to_string())?;
            Ok(ExifApplyOutcome::Parsed)
        }
        Ok(None) => {
            exif_meta::apply_absent(conn, asset_id, "no_exif", "container clean")
                .map_err(|e| e.to_string())?;
            Ok(ExifApplyOutcome::Absent("no_exif".into()))
        }
        Err(e) => {
            let reason = if matches!(e, core_media::MediaError::ExifUnsupported(_)) {
                "unsupported_format"
            } else {
                "parse_failed"
            };
            exif_meta::apply_absent(conn, asset_id, reason, &e.to_string())
                .map_err(|se| se.to_string())?;
            Ok(ExifApplyOutcome::Absent(reason.to_string()))
        }
    }
}

/// core-media::ExifSummary → 派生列（不含 GPS 档位逻辑，见上）。
fn project(s: &core_media::ExifSummary) -> ExifColumns {
    ExifColumns {
        exif_json: s.raw.to_string(),
        taken_at: s.taken_at,
        camera_make: s.make.clone(),
        camera_model: s.model.clone(),
        lens_model: s.lens.clone(),
        iso: s.iso,
        focal_len_mm: s.focal_mm,
        aperture: s.aperture,
        shutter_text: s.shutter.clone(),
        orientation: s.orientation,
        gps_lat: s.gps_lat,
        gps_lon: s.gps_lon,
        gps_source: None,
    }
}

/// 逆地理编码：nominatim 兼容 /reverse（用户可自建 base_url；5s 超时不阻塞队列）。
/// 失败如实报错文本（进 exif json placeReason），绝不静默。
pub fn reverse_geocode(
    provider: &str,
    base_url: &str,
    lat: f64,
    lon: f64,
) -> Result<String, String> {
    match provider {
        "nominatim" => {
            let base = if base_url.trim().is_empty() {
                "https://nominatim.openstreetmap.org"
            } else {
                base_url.trim_end_matches('/')
            };
            let url = format!(
                "{base}/reverse?format=jsonv2&zoom=10&lat={lat}&lon={lon}"
            );
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(5)))
                .http_status_as_error(false)
                .build()
                .into();
            let mut resp = agent
                .get(url)
                .header("User-Agent", "damdesk-exif/0.1 (desktop DAM)")
                .call()
                .map_err(|e| format!("reverse_geocode http: {e}"))?;
            let body: serde_json::Value = resp
                .body_mut()
                .read_json()
                .map_err(|e| format!("reverse_geocode decode: {e}"))?;
            let name = body["display_name"]
                .as_str()
                .or_else(|| body["name"].as_str())
                .ok_or_else(|| "reverse_geocode: 响应无 display_name".to_string())?;
            Ok(name.to_string())
        }
        other => Err(format!("未知 GPS provider: {other}（支持 nominatim 兼容端点）")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../core-storage/src/migrations/0001_init.sql")).unwrap();
        conn.execute_batch(include_str!("../../core-storage/src/migrations/0013_exif_meta.sql")).unwrap();
        conn
    }

    fn seed(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO assets (absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('/x/a.jpg', 'a.jpg', 'jpg', 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn plain_jpeg(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let img = image::DynamicImage::new_rgb8(8, 8);
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        let p = dir.join(name);
        std::fs::write(&p, &buf).unwrap();
        p
    }

    #[test]
    fn no_exif_records_reason_and_never_fails_import() {
        let conn = mem();
        let id = seed(&conn);
        let dir = std::env::temp_dir().join(format!("engine_exif_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = plain_jpeg(&dir, "clean.jpg");
        let out = apply_for_asset(&conn, id, &p, "jpg", &ExifSettings::default()).unwrap();
        assert_eq!(out, ExifApplyOutcome::Absent("no_exif".into()));
        let exif: String = conn
            .query_row("SELECT exif FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert!(exif.contains("no_exif"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_disabled_leaves_exif_null_for_backfill() {
        let conn = mem();
        let id = seed(&conn);
        let dir = std::env::temp_dir().join(format!("engine_exif2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = plain_jpeg(&dir, "clean.jpg");
        let cfg = ExifSettings { parse_on_import: false, ..Default::default() };
        let out = apply_for_asset(&conn, id, &p, "jpg", &cfg).unwrap();
        assert_eq!(out, ExifApplyOutcome::SkippedDisabled);
        let exif: Option<String> = conn
            .query_row("SELECT exif FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(exif, None, "关着解析就不能写任何 exif 状态，留给回填队列");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_format_recorded_as_such() {
        let conn = mem();
        let id = seed(&conn);
        let dir = std::env::temp_dir().join(format!("engine_exif3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.heic");
        std::fs::write(&p, b"not really heic").unwrap();
        let out = apply_for_asset(&conn, id, &p, "heic", &ExifSettings::default()).unwrap();
        assert_eq!(out, ExifApplyOutcome::Absent("unsupported_format".into()));
        let exif: String = conn
            .query_row("SELECT exif FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert!(exif.contains("unsupported_format"), "{exif}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reverse_geocode_unknown_provider_is_loud() {
        let e = reverse_geocode("bogus", "", 0.0, 0.0).unwrap_err();
        assert!(e.contains("bogus"));
    }
}

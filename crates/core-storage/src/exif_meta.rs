//! EXIF2（t_7d9bb323）— 元数据落库：assets 派生列 + exif json 双轨。
//!
//! 双轨契约：**exif json = 真值摘要**（解析器原样输出，含失败原因），
//! **派生列 = 可检索投影**（takenAt/cameraModel/aspectClass/... 由 json 同源的
//! 结构化值写入）。幂等：同一输入重复 apply 结果逐字节一致（UPDATE 全量覆盖）。
//!
//! 隐私红线：GPS 列只在调用方显式给值时写；本模块不读文件、不做任何「兜底补齐」。

use rusqlite::Connection;

use crate::{Result, StorageError};

/// 画幅判据：|ratio−1| ≤ 0.02 → square（±2% 方图容差，验收边界样表见报告）。
pub const SQUARE_TOLERANCE: f64 = 0.02;

/// 写入 assets 的派生列集合（调用方从 core-media::ExifSummary 投影填充；
/// core-storage 不依赖 core-media——分层铁律 L0 同层禁互引）。
#[derive(Debug, Clone, Default)]
pub struct ExifColumns {
    /// exif json 真值摘要（含 parser 标识 / 失败原因）
    pub exif_json: String,
    pub taken_at: Option<i64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens_model: Option<String>,
    pub iso: Option<i64>,
    pub focal_len_mm: Option<f64>,
    pub aperture: Option<f64>,
    pub shutter_text: Option<String>,
    pub orientation: Option<i64>,
    /// 仅当 GPS 档开启且用户选择存坐标时非 None
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    /// 'exif'（存坐标）/ 'exif_place:<provider>'（只存地名）/ None（GPS 档关）
    pub gps_source: Option<String>,
}

/// 全量覆盖写入（幂等）。列与 json 同事务。
pub fn apply_columns(conn: &Connection, asset_id: i64, c: &ExifColumns) -> Result<()> {
    let n = conn.execute(
        "UPDATE assets SET exif=?2, takenAt=?3, cameraMake=?4, cameraModel=?5, lensModel=?6,
         iso=?7, focalLenMm=?8, aperture=?9, shutterText=?10, orientation=?11,
         gps_lat=?12, gps_lon=?13, gpsSource=?14
         WHERE assetId=?1",
        rusqlite::params![
            asset_id,
            c.exif_json,
            c.taken_at,
            c.camera_make,
            c.camera_model,
            c.lens_model,
            c.iso,
            c.focal_len_mm,
            c.aperture,
            c.shutter_text,
            c.orientation,
            c.gps_lat,
            c.gps_lon,
            c.gps_source,
        ],
    )?;
    if n == 0 {
        return Err(StorageError::Other(format!(
            "exif_meta::apply_columns: asset {asset_id} 不存在"
        )));
    }
    Ok(())
}

/// 无 EXIF / 解析失败 / 格式不支持：exif json 记原因（失败可见），派生列清空。
/// 不抛错——这些是「这张图没有元数据」的事实，不是队列故障。
pub fn apply_absent(conn: &Connection, asset_id: i64, reason: &str, detail: &str) -> Result<()> {
    let json = serde_json::json!({
        "parsed": false,
        "reason": reason,
        "detail": detail,
    });
    let n = conn.execute(
        "UPDATE assets SET exif=?2, takenAt=NULL, cameraMake=NULL, cameraModel=NULL,
         lensModel=NULL, iso=NULL, focalLenMm=NULL, aperture=NULL, shutterText=NULL,
         orientation=NULL WHERE assetId=?1",
        rusqlite::params![asset_id, json.to_string()],
    )?;
    if n == 0 {
        return Err(StorageError::Other(format!(
            "exif_meta::apply_absent: asset {asset_id} 不存在"
        )));
    }
    Ok(())
}

/// 宽高 → (aspectRatio, aspectClass)。缺宽高 → (None, None)（解码失败不编造 0/1）。
pub fn aspect_of(w: Option<i64>, h: Option<i64>) -> (Option<f64>, Option<&'static str>) {
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            let ratio = (w as f64 / h as f64 * 1000.0).round() / 1000.0; // 3 位小数
            (Some(ratio), Some(aspect_class(ratio)))
        }
        _ => (None, None),
    }
}

/// 画幅归类：±2% 容差内 = square，w>h = landscape，h>w = portrait。
/// 实现按千分比取整再比（≤20‰），避免 1.02 这类边界值被浮点噪声挤出色。
pub fn aspect_class(ratio: f64) -> &'static str {
    let delta_permille = ((ratio - 1.0) * 1000.0).round().abs();
    if delta_permille <= (SQUARE_TOLERANCE * 1000.0).round() {
        "square"
    } else if ratio > 1.0 {
        "landscape"
    } else {
        "portrait"
    }
}

/// 只回填画幅两列（存量图 backfill：宽高来自既有 resource，不重新解码）。
pub fn set_aspect(conn: &Connection, asset_id: i64, w: Option<i64>, h: Option<i64>) -> Result<()> {
    let (ratio, class) = aspect_of(w, h);
    conn.execute(
        "UPDATE assets SET aspectRatio=?2, aspectClass=?3 WHERE assetId=?1",
        rusqlite::params![asset_id, ratio, class],
    )?;
    Ok(())
}

/// 资产的 primary resource 宽高（backfill 用，不重新解码）。
pub fn primary_dims(conn: &Connection, asset_id: i64) -> Result<(Option<i64>, Option<i64>)> {
    let r = conn.query_row(
        "SELECT width, height FROM resource WHERE assetId=?1 AND role='primary' LIMIT 1",
        rusqlite::params![asset_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    match r {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok((None, None)),
        Err(e) => Err(e.into()),
    }
}

/// 资产 absPath + ext（队列 worker 用）。
pub fn asset_file(conn: &Connection, asset_id: i64) -> Result<Option<(String, String)>> {
    let r = conn.query_row(
        "SELECT absPath, fileExt FROM assets WHERE assetId=?1",
        rusqlite::params![asset_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    match r {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("migrations/0001_init.sql")).unwrap();
        conn.execute_batch(include_str!("migrations/0013_exif_meta.sql")).unwrap();
        conn
    }

    fn seed_asset(conn: &rusqlite::Connection) -> i64 {
        conn.execute(
            "INSERT INTO assets (absPath, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES ('/x/a.jpg', 'a.jpg', 'jpg', 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn aspect_boundaries() {
        // 边界样表（验收 #5）：判定与 ±2% 方图容差
        let cases: [(i64, i64, &str); 8] = [
            (1000, 1000, "square"), // 1:1
            (4000, 3000, "landscape"), // 4:3 = 1.333
            (3000, 4000, "portrait"), // 3:4
            (1920, 1080, "landscape"), // 16:9
            (1080, 1920, "portrait"), // 9:16
            (1020, 1000, "square"),  // 1.02 → 容差内（=0.02 边界含）
            (1021, 1000, "landscape"), // 1.021 → 超容差
            (999, 1000, "square"), // 0.999 → 容差内方图
        ];
        for (w, h, want) in cases {
            let (_, got) = aspect_of(Some(w), Some(h));
            assert_eq!(got, Some(want), "{w}x{h}");
        }
        // 解码失败：NULL 不编造
        assert_eq!(aspect_of(None, Some(100)), (None, None));
        assert_eq!(aspect_of(Some(0), Some(0)), (None, None));
    }

    #[test]
    fn apply_columns_idempotent() {
        let conn = mem();
        let id = seed_asset(&conn);
        let c = ExifColumns {
            exif_json: r#"{"parser":"t"}"#.into(),
            taken_at: Some(1714537230),
            camera_make: Some("Apple".into()),
            camera_model: Some("iPhone 14 Pro".into()),
            iso: Some(1250),
            orientation: Some(6),
            ..Default::default()
        };
        apply_columns(&conn, id, &c).unwrap();
        let before: String = conn
            .query_row("SELECT exif || '|' || takenAt || '|' || cameraModel FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        apply_columns(&conn, id, &c).unwrap();
        let after: String = conn
            .query_row("SELECT exif || '|' || takenAt || '|' || cameraModel FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "重跑必须零变化");
    }

    #[test]
    fn absent_records_reason_and_clears_columns() {
        let conn = mem();
        let id = seed_asset(&conn);
        let c = ExifColumns { taken_at: Some(7), camera_model: Some("X".into()), ..Default::default() };
        apply_columns(&conn, id, &c).unwrap();
        apply_absent(&conn, id, "no_exif", "container clean").unwrap();
        let (exif, taken): (String, Option<i64>) = conn
            .query_row("SELECT exif, takenAt FROM assets WHERE assetId=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(exif.contains("no_exif"));
        assert_eq!(taken, None);
        let model: Option<String> = conn
            .query_row("SELECT cameraModel FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(model, None);
    }

    #[test]
    fn set_aspect_from_resource_dims() {
        let conn = mem();
        let id = seed_asset(&conn);
        conn.execute(
            "INSERT INTO resource (assetId, role, width, height) VALUES (?1, 'primary', 3000, 4000)",
            [id],
        )
        .unwrap();
        let (w, h) = primary_dims(&conn, id).unwrap();
        assert_eq!((w, h), (Some(3000), Some(4000)));
        set_aspect(&conn, id, w, h).unwrap();
        let cls: String = conn
            .query_row("SELECT aspectClass FROM assets WHERE assetId=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(cls, "portrait");
    }

    #[test]
    fn missing_asset_is_loud() {
        let conn = mem();
        let err = apply_columns(&conn, 999, &ExifColumns::default());
        assert!(err.is_err());
    }
}

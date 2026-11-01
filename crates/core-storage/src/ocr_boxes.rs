//! DM-C1（t_d5c6c6a2）：OCR 文本行框的落库与读取（migration 0034）。
//!
//! ## 谁写、谁读
//!
//! * 写：`svc-pipeline::ocr_queue::ocr_one`（识别成功即与 `assets.ocr_text` 同事务替换整组）；
//! * 读：`svc-pipeline` 的 inpaint 重绘区域决议（`inpaint_region`）——把 VL 返回的 bbox
//!   与本地 OCR 实测行框交叉验证，重叠不过半就拒绝该区域（不拿模型幻觉去重绘整幅图）。
//!
//! ## 为什么归一化坐标
//!
//! 消费方（VL regions / inpaint box）全是 0..1 相对坐标；存像素就得在每次消费时重新取宽高，
//! 宽度缺失的历史资产会直接读不出框。归一化在写入口（那里正好有解码宽高）一次消化掉。
//!
//! ## 幂等
//!
//! 一组框 = 一次识别的结果，重跑必然整体替换（先 DELETE 再 INSERT，同一事务）——
//! 不做「按 idx upsert」，否则上次 7 行、这次 4 行时会留下 3 行幽灵框。

use rusqlite::{params, Connection};

use crate::{Result, StorageError};

/// 一行 OCR 文本行框（归一化 0..1）。
#[derive(Debug, Clone, PartialEq)]
pub struct OcrBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    /// rec 行文本（可能为空 = 检出框但无字符）
    pub text: String,
    /// DB det 框得分（bbox 区域均值）
    pub box_score: f64,
    /// CTC 字符置信度均值
    pub text_score: f64,
}

impl OcrBox {
    /// 归一化矩形 `[x0,y0,x1,y1]`（重绘区域决议的输入形态）。
    pub fn rect(&self) -> [f64; 4] {
        [self.x0, self.y0, self.x1, self.y1]
    }
}

/// 像素坐标 → 归一化（宽高为 0 时按整幅处理：坐标 clamp 到 0..1，不让除零炸穿识别）。
pub fn normalize_box(x0: i64, y0: i64, x1: i64, y1: i64, w: u32, h: u32) -> [f64; 4] {
    let (fw, fh) = (
        if w == 0 { 1.0 } else { f64::from(w) },
        if h == 0 { 1.0 } else { f64::from(h) },
    );
    [
        (x0 as f64 / fw).clamp(0.0, 1.0),
        (y0 as f64 / fh).clamp(0.0, 1.0),
        (x1 as f64 / fw).clamp(0.0, 1.0),
        (y1 as f64 / fh).clamp(0.0, 1.0),
    ]
}

/// 整组替换某资产的行框（同一事务；空组 = 清空）。返回写入行数。
pub fn replace(conn: &Connection, asset_id: i64, boxes: &[OcrBox]) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM asset_ocr_boxes WHERE assetId = ?1", [asset_id])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO asset_ocr_boxes (assetId, idx, x0, y0, x1, y1, text, boxScore, textScore)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for (i, b) in boxes.iter().enumerate() {
            stmt.execute(params![
                asset_id,
                i as i64,
                b.x0,
                b.y0,
                b.x1,
                b.y1,
                b.text,
                b.box_score,
                b.text_score
            ])?;
        }
    }
    tx.commit()?;
    Ok(boxes.len())
}

/// 读某资产的全部行框（阅读序 = idx 升序）。
pub fn load(conn: &Connection, asset_id: i64) -> Result<Vec<OcrBox>> {
    let mut stmt = conn.prepare(
        "SELECT x0, y0, x1, y1, text, boxScore, textScore FROM asset_ocr_boxes
         WHERE assetId = ?1 ORDER BY idx",
    )?;
    let rows = stmt.query_map([asset_id], |r| {
        Ok(OcrBox {
            x0: r.get(0)?,
            y0: r.get(1)?,
            x1: r.get(2)?,
            y1: r.get(3)?,
            text: r.get(4)?,
            box_score: r.get(5)?,
            text_score: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 只取矩形（重绘区域决议用；不需要文本/得分）。
pub fn load_rects(conn: &Connection, asset_id: i64) -> Result<Vec<[f64; 4]>> {
    Ok(load(conn, asset_id)?.iter().map(OcrBox::rect).collect())
}

/// 行框行数（自检/验收口径：`ocr_text != ''` 的资产应当有框，反之亦然）。
pub fn count(conn: &Connection, asset_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM asset_ocr_boxes WHERE assetId = ?1",
        [asset_id],
        |r| r.get(0),
    )?)
}

/// 入参校验：坐标必须有限且在 0..1（写入口用，脏数据不许入库）。
pub fn validate(b: &OcrBox) -> Result<()> {
    let vals = [b.x0, b.y0, b.x1, b.y1];
    if vals.iter().any(|v| !v.is_finite() || *v < 0.0 || *v > 1.0) {
        return Err(StorageError::Invalid(format!(
            "ocr box 坐标非归一化: [{}, {}, {}, {}]",
            b.x0, b.y0, b.x1, b.y1
        )));
    }
    if b.x1 <= b.x0 || b.y1 <= b.y0 {
        return Err(StorageError::Invalid(format!(
            "ocr box 退化矩形（宽/高 <= 0）: [{}, {}, {}, {}]",
            b.x0, b.y0, b.x1, b.y1
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO assets (assetId, absPath, mediaType, fileName, fileExt, fileSz, mtime, importedAt)
             VALUES (1, '/tmp/a.jpg', 'image', 'a.jpg', 'jpg', 1, 0, 0)",
            [],
        )
        .unwrap();
        conn
    }

    fn box_of(x0: f64, y0: f64, x1: f64, y1: f64, text: &str) -> OcrBox {
        OcrBox {
            x0,
            y0,
            x1,
            y1,
            text: text.into(),
            box_score: 0.9,
            text_score: 0.8,
        }
    }

    #[test]
    fn replace_is_idempotent_and_load_keeps_reading_order() {
        let conn = db();
        let first = vec![
            box_of(0.10, 0.10, 0.40, 0.20, "水印A"),
            box_of(0.15, 0.60, 0.55, 0.70, "水印B"),
        ];
        assert_eq!(replace(&conn, 1, &first).unwrap(), 2);
        assert_eq!(count(&conn, 1).unwrap(), 2);
        let got = load(&conn, 1).unwrap();
        assert_eq!(got, first, "读回顺序 = 写入顺序（idx）");

        // 重跑识别：这次只有 1 行 —— 旧的第二行必须消失（不做按 idx upsert）
        let second = vec![box_of(0.2, 0.2, 0.3, 0.3, "只剩一行")];
        assert_eq!(replace(&conn, 1, &second).unwrap(), 1);
        assert_eq!(count(&conn, 1).unwrap(), 1);
        assert_eq!(load_rects(&conn, 1).unwrap(), vec![[0.2, 0.2, 0.3, 0.3]]);

        // 空组 = 清空（识别返回无字也要能把旧框清掉）
        replace(&conn, 1, &[]).unwrap();
        assert_eq!(count(&conn, 1).unwrap(), 0);
    }

    #[test]
    fn deleting_asset_cascades_boxes() {
        let conn = db();
        replace(&conn, 1, &[box_of(0.1, 0.1, 0.2, 0.2, "x")]).unwrap();
        conn.execute("DELETE FROM assets WHERE assetId = 1", []).unwrap();
        assert_eq!(count(&conn, 1).unwrap(), 0);
    }

    #[test]
    fn normalize_maps_pixels_and_clamps() {
        assert_eq!(normalize_box(50, 25, 100, 75, 200, 100), [0.25, 0.25, 0.5, 0.75]);
        // 越界（unclip 外扩可能出界）→ clamp 到 0..1
        assert_eq!(normalize_box(-10, -5, 300, 120, 200, 100), [0.0, 0.0, 1.0, 1.0]);
        // 宽高缺失（0）不炸：按整幅 1 处理
        assert_eq!(normalize_box(0, 0, 10, 10, 0, 0), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn validate_rejects_degenerate_and_unnormalized() {
        assert!(validate(&box_of(0.1, 0.1, 0.2, 0.2, "ok")).is_ok());
        assert!(validate(&box_of(0.2, 0.2, 0.2, 0.3, "退化")).is_err());
        assert!(validate(&box_of(-0.1, 0.1, 0.2, 0.2, "负")).is_err());
        assert!(validate(&box_of(0.1, 0.1, 1.2, 0.2, "越界")).is_err());
    }
}

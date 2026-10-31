//! `result_columns` —— 插件声明的行业结果列（协议 §4.5 / 结果行契约第 2 条）。
//!
//! 插件在 `resources/result_columns.json` 里声明自己要展示的列，宿主**只按 schema 画表格**
//! （排序/筛选/导出同一份口径），新增行业列不改前端代码。
//!
//! 文件形态（两种都收，省得包作者踩格式坑）：
//!
//! ```jsonc
//! // A. 裸数组（推荐）
//! [{ "key": "score", "label": "白底偏差", "type": "number", "unit": "%", "order": 1 }]
//! // B. 包一层
//! { "result_columns": [ ... ] }
//! ```
//!
//! 硬边界（契约第 2 条）：
//! * `type` 是闭集 `number|text|enum|bool` —— 自造类型前端不可渲染；
//! * `key` 必须是非空字符串且**唯一**（行里就是按 key 取 `fields[key]`）；
//! * 未声明 → `None`（宿主用默认列）；**声明了但全部非法 → 也回落 `None` 并给出 warnings**，
//!   绝不因为一个坏列让整页报错（「不许报错」是契约原文）。
//!
//! 这里是纯解析，不碰 DB、不碰 UI；宿主各处共用这一份判定。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 列类型闭集（自造类型 = 前端不可渲染）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    Number,
    Text,
    Enum,
    Bool,
}

impl ColumnType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Text => "text",
            Self::Enum => "enum",
            Self::Bool => "bool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "number" => Some(Self::Number),
            "text" => Some(Self::Text),
            "enum" => Some(Self::Enum),
            "bool" => Some(Self::Bool),
            _ => None,
        }
    }

    pub fn all() -> [&'static str; 4] {
        ["number", "text", "enum", "bool"]
    }
}

/// 一列声明（宿主渲染的唯一依据）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultColumn {
    /// 行里 `fields` 的键
    pub key: String,
    /// 表头文字（插件作者用自己的行业术语；i18n-F：按请求 locale 取值后的展示串）
    pub label: String,
    /// 表头的**原样 i18n 对象**（i18n-F：`label` 写成 `{"locale": 文案}` 时透传给前端，
    /// 切语言不用重拉；字符串形态 = `None`）
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "labelI18n")]
    pub label_i18n: Option<core_config::packs::LocalizedText>,
    #[serde(rename = "type")]
    pub kind: ColumnType,
    /// 单位（`%` / `px` / `s`…）；只用于展示
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// 展示顺序（升序；缺省 0，同值按声明顺序稳定）
    #[serde(default)]
    pub order: i64,
}

/// 解析结果：`None` = 没有可用声明（未声明 / 全非法）→ 宿主用默认列。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedColumns {
    pub columns: Option<Vec<ResultColumn>>,
    /// 被丢掉的项与原因（装机日志/接口可选回显；**不影响渲染**）
    pub warnings: Vec<String>,
}

/// resources 里声明结果列的文件名（协议 §4.5；缺省无此文件 = 默认列）。
pub const RESULT_COLUMNS_FILE: &str = "result_columns.json";

/// 解析 `result_columns` 声明（裸数组或 `{result_columns:[...]}` 两种形态）。
///
/// i18n-F：`label` 接受字符串（全语言通用）或对象 `{"locale": 文案}`；`locale` 参数是
/// 请求侧语言（Accept-Language 归一后的 `zh-Hans`/`en-US`），取值回退链见
/// [`core_config::packs::LocalizedText::pick`]。原样对象存进 [`ResultColumn::label_i18n`]。
pub fn parse_result_columns(v: &serde_json::Value, locale: &str) -> ParsedColumns {
    let mut warnings: Vec<String> = Vec::new();
    let arr = if let Some(a) = v.as_array() {
        a.clone()
    } else if let Some(a) = v.get("result_columns").and_then(|x| x.as_array()) {
        a.clone()
    } else if let Some(a) = v.get("columns").and_then(|x| x.as_array()) {
        a.clone()
    } else {
        return ParsedColumns {
            columns: None,
            warnings: vec!["result_columns 必须是数组（或 {\"result_columns\":[...]}）".into()],
        };
    };

    let mut out: Vec<ResultColumn> = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let key = item.get("key").and_then(|x| x.as_str()).unwrap_or("").trim();
        if key.is_empty() {
            warnings.push(format!("第 {} 项缺 key，已忽略", i + 1));
            continue;
        }
        if out.iter().any(|c| c.key == key) {
            warnings.push(format!("key {key:?} 重复声明，保留第一条"));
            continue;
        }
        let lt = core_config::packs::LocalizedText::from_json(item.get("label"));
        let picked = lt.pick(locale).trim();
        let label = if picked.is_empty() { key } else { picked };
        let label_i18n = lt.as_map().map(|_| lt.clone());
        let raw_type = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let Some(kind) = ColumnType::parse(raw_type) else {
            warnings.push(format!(
                "{key:?} 的 type {raw_type:?} 不在闭集（可用 {:?}），已忽略",
                ColumnType::all()
            ));
            continue;
        };
        out.push(ResultColumn {
            key: key.to_string(),
            label: label.to_string(),
            label_i18n,
            kind,
            unit: item
                .get("unit")
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            order: item.get("order").and_then(serde_json::Value::as_i64).unwrap_or(0),
        });
    }

    if out.is_empty() {
        // 全非法 = 当作未声明（默认列），不报错
        if warnings.is_empty() {
            warnings.push("result_columns 为空数组：按未声明处理（默认列）".into());
        }
        return ParsedColumns {
            columns: None,
            warnings,
        };
    }
    // 稳定排序：order 升序，同 order 保持声明顺序
    out.sort_by_key(|c| c.order);
    ParsedColumns {
        columns: Some(out),
        warnings,
    }
}

/// 从包目录读 `resources/result_columns.json`（读不到/解析失败 = `None` + warning，不报错）。
pub fn load_result_columns(pack_dir: &Path, locale: &str) -> ParsedColumns {
    let path = pack_dir.join("resources").join(RESULT_COLUMNS_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ParsedColumns::default();
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => parse_result_columns(&v, locale),
        Err(e) => ParsedColumns {
            columns: None,
            warnings: vec![format!("{} 不是合法 JSON：{e}", path.display())],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bare_array_is_parsed_and_sorted_by_order() {
        let p = parse_result_columns(&json!([
            {"key": "dev", "label": "白底偏差", "type": "number", "unit": "%", "order": 2},
            {"key": "bg", "label": "背景", "type": "enum", "order": 1},
        ]), "zh-Hans");
        let cols = p.columns.unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].key, "bg", "order 小的排前面");
        assert_eq!(cols[1].key, "dev");
        assert_eq!(cols[1].kind, ColumnType::Number);
        assert_eq!(cols[1].unit.as_deref(), Some("%"));
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn wrapped_object_form_is_accepted() {
        let p = parse_result_columns(&json!({
            "result_columns": [{"key": "ratio", "label": "占比", "type": "number"}]
        }), "zh-Hans");
        assert_eq!(p.columns.unwrap()[0].key, "ratio");
    }

    #[test]
    fn invalid_entries_are_dropped_with_reasons_not_fatal() {
        let p = parse_result_columns(&json!([
            {"label": "没 key", "type": "number"},
            {"key": "selfmade", "type": "score"},          // 自造类型
            {"key": "ok_col", "type": "text"},
            {"key": "ok_col", "type": "number"},           // 重复 key
        ]), "zh-Hans");
        let cols = p.columns.unwrap();
        assert_eq!(cols.len(), 1, "只留合法的那一列");
        assert_eq!(cols[0].key, "ok_col");
        assert_eq!(cols[0].kind, ColumnType::Text, "重复 key 保留第一条");
        assert_eq!(p.warnings.len(), 3, "坏项逐个给原因：{:#?}", p.warnings);
    }

    #[test]
    fn all_invalid_or_empty_falls_back_to_default_columns() {
        for v in [json!([]), json!([{"key": "x", "type": "wrong"}]), json!({"nope": 1})] {
            let p = parse_result_columns(&v, "zh-Hans");
            assert!(p.columns.is_none(), "必须回落默认列（不许报错）：{v}");
            assert!(!p.warnings.is_empty());
        }
    }

    #[test]
    fn missing_file_means_no_declaration() {
        let dir = std::env::temp_dir().join("dam-n5b-cols-empty");
        std::fs::create_dir_all(dir.join("resources")).unwrap();
        let p = load_result_columns(&dir, "zh-Hans");
        assert!(p.columns.is_none());
        assert!(p.warnings.is_empty(), "没文件不是错误（默认列）");
    }

    #[test]
    fn file_is_read_from_resources() {
        let dir = std::env::temp_dir().join("dam-n5b-cols-file");
        std::fs::create_dir_all(dir.join("resources")).unwrap();
        std::fs::write(
            dir.join("resources").join(RESULT_COLUMNS_FILE),
            r#"[{"key":"cost","label":"耗时","type":"number","unit":"ms","order":1}]"#,
        )
        .unwrap();
        let p = load_result_columns(&dir, "zh-Hans");
        assert_eq!(p.columns.unwrap()[0].label, "耗时");
    }

    #[test]
    fn label_accepts_localized_object_with_fallback() {
        // i18n-F：label 对象形态按请求 locale 取值，原样对象透传 label_i18n；
        // 字符串形态 = 全语言通用（label_i18n = None），缺语言回退 zh-Hans。
        let p = parse_result_columns(
            &json!([
                {"key": "dev", "label": {"zh-Hans": "白底偏差", "en-US": "White deviation"}, "type": "number"},
                {"key": "bg", "label": "背景", "type": "enum"},
            ]),
            "en-US",
        );
        let cols = p.columns.unwrap();
        assert_eq!(cols[0].label, "White deviation");
        assert_eq!(
            cols[0].label_i18n.as_ref().unwrap().pick("zh-Hans"),
            "白底偏差"
        );
        assert_eq!(cols[1].label, "背景", "字符串形态行为不变");
        assert!(cols[1].label_i18n.is_none());

        let p_zh = parse_result_columns(
            &json!([{"key": "dev", "label": {"zh-Hans": "白底偏差"}, "type": "number"}]),
            "en-US",
        );
        let cols = p_zh.columns.unwrap();
        assert_eq!(cols[0].label, "白底偏差", "缺 en → 回退 zh-Hans，不崩");
        // label 缺失/空 → 回退 key（老行为）
        let p_key = parse_result_columns(
            &json!([{"key": "ratio", "type": "number"}]),
            "en-US",
        );
        assert_eq!(p_key.columns.unwrap()[0].label, "ratio");
    }
}

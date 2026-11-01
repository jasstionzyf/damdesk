//! `DamaiTagProvider` — dam-ai `/v1/tagging` 打标 provider（R7 自托管默认）。
//!
//! R7（2026-10-14）：dam-ai 只是默认 provider 不是特权路径——本实现与
//! [`crate::openai_compat::OpenAiCompatTagProvider`] 平级，同一 TagProvider trait。
//! dam-ai 服务端把 TagBackend（vLLM/openai_compat）藏在 /v1/tagging 后面，
//! 本地只面向它的稳定契约：
//!
//! - inline 通道（app-owned 模板语义，R7「模板/预设两端对齐」）：app 的三合一
//!   prompt 模板 / 自定义任务 prompt + 输出 schema 直接进请求，服务端 vLLM
//!   guided decoding 按 schema 强约束输出，逐项 schema 校验后回 `output` 对象。
//! - 图片输入 = `image_url` 字符串，base64 data URL 与 http(s) URL 均可
//!   （vLLM 兼容 OpenAI 多模态形态）；与 openai_compat 同用 `image_data_url`。
//! - 连通性测试 = `GET {base_url}/models`（dam-ai 聚合视图），零 token 消耗。
//!
//! 请求形态（inline）：
//! ```json
//! {"prompt":"<模板>","schema":{...},"model":"qwen3.5-4b",
//!  "inputs":[{"id":"1","image_url":"data:image/jpeg;base64,..."}],
//!  "params":{"temperature":0.1,"max_tokens":1024}}
//! ```
//! 响应：`results[0] = {"id","status":"ok","output":{...}}` 或
//! `{"id","status":"error","code","error"}`（逐项状态，批量内互不拖垮）。

use std::sync::atomic::{AtomicBool, Ordering};

use core_config::{ModelSpec, TagSettings};

use crate::http::image_data_url;
use crate::{parse_inline_value, parse_tagging_output, Result, TagError, TagProvider, TaggingOutput};

/// 简单 `{"type": <t>}` 节点。
#[derive(serde::Serialize)]
struct SchemaType {
    #[serde(rename = "type")]
    type_: &'static str,
}

#[derive(serde::Serialize)]
struct TagItemProperties {
    name: SchemaType,
    score: SchemaType,
}

#[derive(serde::Serialize)]
struct TagItemSchema {
    #[serde(rename = "type")]
    type_: &'static str,
    properties: TagItemProperties,
    required: [&'static str; 1],
    #[serde(rename = "additionalProperties")]
    additional_properties: bool,
}

#[derive(serde::Serialize)]
struct TagsArraySchema {
    #[serde(rename = "type")]
    type_: &'static str,
    items: TagItemSchema,
}

/// 三合一属性。声明顺序 = wire 顺序 = 模板呈现顺序（title 优先）。
///
/// 设计裁决（W4 验收根因）：vLLM xgrammar 按 schema `properties` 的**出现顺序**
/// 约束生成。`serde_json::json!` 产出的 Value 是 BTreeMap，键按字母序落盘
/// （description 打头、title 垫底）→ 模型按 description→tags 写完后面对
/// title_ch 缺位退化成无限刷空白 token，直到 max_tokens 截断 → JSON 无根闭括号
/// → 全量 `output_invalid`。改用 serde derive 结构体（字段序即 wire 序），
/// properties 顺序与 TAG_PROMPT_TEMPLATE 呈现顺序严格一致。
#[derive(serde::Serialize)]
pub struct ThreeInOneProperties {
    title_ch: SchemaType,
    title_en: SchemaType,
    description: SchemaType,
    tags: TagsArraySchema,
}

/// 三合一输出 schema（title_ch/title_en/description/tags）——inline 通道
/// guided decoding 用；tags = [{name,score}]（与 TAG_PROMPT_TEMPLATE 的 JSON
/// 约定一致，from_json 本就吃对象形态；不用 anyOf——vLLM xgrammar 对
/// anyOf/object 变体支持不稳，保持 schema 与 prompt 严格同形）。
#[derive(serde::Serialize)]
pub struct ThreeInOneSchema {
    #[serde(rename = "type")]
    type_: &'static str,
    properties: ThreeInOneProperties,
    required: [&'static str; 4],
    #[serde(rename = "additionalProperties")]
    additional_properties: bool,
}

/// inline 短文本输出 schema（自定义打标任务；空内容约定 → 空字符串）。
#[derive(serde::Serialize)]
pub struct InlineValueSchema {
    #[serde(rename = "type")]
    type_: &'static str,
    properties: InlineValueProperties,
    required: [&'static str; 1],
    #[serde(rename = "additionalProperties")]
    additional_properties: bool,
}

#[derive(serde::Serialize)]
struct InlineValueProperties {
    value: SchemaType,
}

/// inline 通道两种输出 schema 的和型（wire 上直接内联序列化）。
#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum InlineSchema {
    ThreeInOne(ThreeInOneSchema),
    ShortValue(InlineValueSchema),
}

pub fn three_in_one_schema() -> ThreeInOneSchema {
    let str_ = || SchemaType { type_: "string" };
    ThreeInOneSchema {
        type_: "object",
        properties: ThreeInOneProperties {
            title_ch: str_(),
            title_en: str_(),
            description: str_(),
            tags: TagsArraySchema {
                type_: "array",
                items: TagItemSchema {
                    type_: "object",
                    properties: TagItemProperties {
                        name: str_(),
                        score: SchemaType { type_: "number" },
                    },
                    required: ["name"],
                    additional_properties: false,
                },
            },
        },
        required: ["title_ch", "title_en", "description", "tags"],
        additional_properties: false,
    }
}

pub fn inline_value_schema() -> InlineValueSchema {
    InlineValueSchema {
        type_: "object",
        properties: InlineValueProperties {
            value: SchemaType { type_: "string" },
        },
        required: ["value"],
        additional_properties: false,
    }
}

/// `/v1/tagging` inline 请求体（typed：schema 序列化保序的关键路径）。
#[derive(serde::Serialize)]
struct TaggingRequest<'a> {
    prompt: &'a str,
    schema: &'a InlineSchema,
    model: &'a str,
    inputs: [TaggingInput<'a>; 1],
    params: TaggingParams,
}

#[derive(serde::Serialize)]
struct TaggingInput<'a> {
    id: &'a str,
    image_url: &'a str,
}

#[derive(serde::Serialize)]
struct TaggingParams {
    temperature: f64,
    max_tokens: u32,
}

/// OpenAI 兼容云 VL 打标 provider（dam-ai /v1/tagging 形态）。
pub struct DamaiTagProvider {
    spec: ModelSpec,
    cfg: TagSettings,
    agent: ureq::Agent,
    loaded: AtomicBool,
}

impl DamaiTagProvider {
    pub fn new(spec: ModelSpec, cfg: TagSettings) -> Self {
        let agent = crate::http::agent_with_proxy(cfg.timeout_ms / 1000 + 1, cfg.proxy.as_deref());
        Self {
            spec,
            cfg,
            agent,
            loaded: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> &TagSettings {
        &self.cfg
    }

    /// 用户开关 + 配置齐备（UI 判断「启用 AI 打标」是否可用）。
    pub fn usable(&self) -> bool {
        self.cfg.enabled && self.cfg.configured()
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.cfg.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn get_json(&self, path: &str) -> Result<serde_json::Value> {
        let url = self.endpoint(path);
        let mut req = self.agent.get(&url);
        if !self.cfg.api_key.trim().is_empty() {
            req = req.header(
                "authorization",
                &format!("Bearer {}", self.cfg.api_key.trim()),
            );
        }
        let mut resp = req
            .call()
            .map_err(|e| TagError::Http(format!("GET {url}: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| TagError::Http(format!("read {url}: {e}")))?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            TagError::Http(format!("bad json from {url} (status {status}): {e}; body={text}"))
        })?;
        if !(200..300).contains(&status) {
            let msg = parsed
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(TagError::Upstream(format!("HTTP {status} from {url}: {msg}")));
        }
        Ok(parsed)
    }

    fn post_json<T: serde::Serialize>(&self, path: &str, body: &T) -> Result<serde_json::Value> {
        let url = self.endpoint(path);
        let mut req = self
            .agent
            .post(&url)
            .header("content-type", "application/json");
        if !self.cfg.api_key.trim().is_empty() {
            req = req.header(
                "authorization",
                &format!("Bearer {}", self.cfg.api_key.trim()),
            );
        }
        let mut resp = req
            .send_json(body)
            .map_err(|e| TagError::Http(format!("POST {url}: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| TagError::Http(format!("read {url}: {e}")))?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            TagError::Http(format!("bad json from {url} (status {status}): {e}; body={text}"))
        })?;
        if !(200..300).contains(&status) {
            let msg = parsed
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    parsed
                        .get("error")
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| text.chars().take(300).collect())
                });
            return Err(TagError::Upstream(format!("HTTP {status} from {url}: {msg}")));
        }
        Ok(parsed)
    }

    /// inline 通道单图请求：prompt + schema + 单输入；params 透传（lock_params=false）。
    /// body 走 typed 序列化——schema 的 properties 顺序必须原样落 wire（见
    /// [`ThreeInOneProperties`] 的设计裁决，BTreeMap 字母序会打碎 xgrammar 生成序）。
    fn tagging_inline(
        &self,
        prompt: &str,
        schema: InlineSchema,
        image: serde_json::Value,
        max_tokens: u32,
    ) -> Result<serde_json::Value> {
        let img_str = image.as_str().ok_or_else(|| {
            TagError::Parse(format!("inline image must be a string, got {image}"))
        })?;
        let body = TaggingRequest {
            prompt,
            schema: &schema,
            model: &self.cfg.model,
            inputs: [TaggingInput { id: "1", image_url: img_str }],
            params: TaggingParams {
                temperature: 0.1,
                max_tokens,
            },
        };
        let resp = self.post_json("/tagging", &body)?;
        if let Some(err) = resp.get("error") {
            return Err(TagError::Upstream(err.to_string()));
        }
        let item = resp
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| TagError::Parse(format!("no results[0] in {resp}")))?;
        if item.get("status").and_then(|s| s.as_str()) != Some("ok") {
            let msg = item
                .get("error")
                .and_then(|e| e.as_str())
                .or_else(|| item.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()))
                .unwrap_or("unknown tagging error")
                .to_string();
            // guided decoding 出不了合法 JSON = 生成被预算砍断（C8/C9 实测口径）。
            // 分类成 truncated 并带上本次实际下发的 max_tokens，UI 才能指向预算。
            let code = item.get("code").and_then(|c| c.as_str()).unwrap_or("");
            let budget_hit = code == "output_invalid"
                || msg.contains("not valid JSON")
                || msg.contains("truncat");
            if budget_hit {
                return Err(TagError::Truncated {
                    max_tokens,
                    budget: self.cfg.max_tokens_label(),
                    detail: format!(
                        "dam-ai {code}: {msg}（guided decoding 未收尾，常见原因是 max_tokens 预算不足）"
                    ),
                });
            }
            return Err(TagError::Upstream(msg));
        }
        Ok(item
            .get("output")
            .cloned()
            .ok_or_else(|| TagError::Parse(format!("results[0].output missing in {resp}")))?)
    }
}

impl TagProvider for DamaiTagProvider {
    fn load(&mut self) -> Result<()> {
        if !self.cfg.configured() {
            return Err(TagError::NotConfigured(
                "dam-ai 打标未配置：请在设置里填写 /v1/tagging base_url 与模型名".to_string(),
            ));
        }
        self.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn tag_image(&self, path: &std::path::Path) -> Result<TaggingOutput> {
        if !self.ready() {
            return Err(TagError::NotLoaded(self.spec.name.clone()));
        }
        let (bytes, mime) = crate::vl_image::load_for_vl(path)?;
        let img = image_data_url(&bytes, &mime);
        let output = self.tagging_inline(
            &self.cfg.prompt_template,
            InlineSchema::ThreeInOne(three_in_one_schema()),
            serde_json::json!(img),
            self.cfg.effective_max_tokens(),
        )?;
        // guided decoding + 服务端 schema 校验 → output 应为对象；字符串形态
        // 兜底走围栏/废话容忍解析。
        match &output {
            serde_json::Value::Object(_) => Ok(crate::tagging_output_from_value(&output)),
            serde_json::Value::String(s) => parse_tagging_output(s),
            _ => Err(TagError::Parse(format!(
                "unexpected output shape: {output}"
            ))),
        }
    }

    fn test_connection(&self) -> Result<String> {
        if !self.cfg.configured() {
            return Err(TagError::NotConfigured("endpoint/模型名未填写".to_string()));
        }
        // 零 token 探活：dam-ai 聚合 /v1/models（后端 vLLM 不可达时 502 可读报错）
        let parsed = self.get_json("/models")?;
        let ids: Vec<String> = parsed
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|x| x.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            return Err(TagError::Upstream(format!(
                "{}/models 已连通但模型列表为空（vLLM 后端未就绪？）",
                self.cfg.base_url.trim_end_matches('/')
            )));
        }
        Ok(ids.join(", "))
    }

    /// D10 inline prompt 通道（旧单值通道）：输出约束模板包用户 prompt；schema 强约束
    /// {"value": string}，归一化交给 [`crate::parse_inline_value`]。
    fn tag_inline(&self, path: &std::path::Path, user_prompt: &str) -> Result<String> {
        if !self.ready() {
            return Err(TagError::NotLoaded(self.spec.name.clone()));
        }
        let (bytes, mime) = crate::vl_image::load_for_vl(path)?;
        let img = image_data_url(&bytes, &mime);
        let output = self.tagging_inline(
            &crate::build_inline_prompt(user_prompt),
            InlineSchema::ShortValue(inline_value_schema()),
            serde_json::json!(img),
            self.cfg.effective_max_tokens().min(256),
        )?;
        let value = match &output {
            serde_json::Value::Object(_) => output
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => String::new(),
        };
        Ok(parse_inline_value(&value))
    }

    /// 统一标签通道（t_d65d55a1）：dam-ai /v1/tagging 服务端 inline schema 仍是
    /// {"value": string}（数组 schema 属服务端改动，不在本卡范围；服务端在数组
    /// 契约下的真实返回形态**未验证**）。客户端兼容做法：仍走 ShortValue schema
    /// （guided decoding 保证结构可解析），模型按新 prompt 把数组/分号串写进 value
    /// 字段，取回后过 [`crate::parse_inline_values`]（数组 / 分号串都能吃）。
    fn tag_inline_values(&self, path: &std::path::Path, user_prompt: &str) -> Result<Vec<String>> {
        let s = self.tag_inline(path, user_prompt)?;
        Ok(crate::parse_inline_values(&s))
    }

    fn ready(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TagProvider;

    fn cfg() -> TagSettings {
        TagSettings {
            enabled: true,
            provider: core_config::TAG_PROVIDER_DAMAI.into(),
            base_url: "http://127.0.0.1:9/v1".into(), // discard 端口，必然连不上
            api_key: String::new(),
            model: "qwen3.5-4b".into(),
            prompt_template: crate::TAG_PROMPT_TEMPLATE.into(),
            max_tokens: 1024,
            timeout_ms: 2_000,
            proxy: Some(String::new()),
        }
    }

    fn spec() -> ModelSpec {
        ModelSpec {
            name: "openai-compat-tagger:damai:qwen3.5-4b".into(),
            kind: core_config::SpecKind::Tagging,
            dims: 0,
            modalities: vec!["image".into()],
            normalized: false,
            provider: "damai".into(),
            preprocess: Default::default(),
        }
    }

    #[test]
    fn schemas_match_prompt_contract() {
        // wire 顺序断言：xgrammar 按 schema properties 的出现顺序约束生成，
        // 顺序必须 = 模板呈现顺序（title 优先）。BTreeMap 字母序（description
        // 打头）会让模型在 title 缺位时无限刷空白直到 max_tokens 截断
        // （W4 验收根因，2026-10-15）——此测试钉死回归。
        let s = serde_json::to_string(&three_in_one_schema()).unwrap();
        let pos = |k: &str| s.find(&format!("\"{k}\":")).unwrap_or_else(|| panic!("missing {k} in {s}"));
        assert!(pos("title_ch") < pos("title_en"), "wire order: {s}");
        assert!(pos("title_en") < pos("description"), "wire order: {s}");
        assert!(pos("description") < pos("tags"), "wire order: {s}");
        for k in ["title_ch", "title_en", "description", "tags"] {
            assert!(s.contains(k), "missing {k}");
        }
        let v = serde_json::to_string(&inline_value_schema()).unwrap();
        assert!(v.contains("\"required\":[\"value\"]"), "wire: {v}");
    }

    #[test]
    fn tagging_request_serializes_schema_in_order() {
        // 端到端 wire 形态：typed 请求体里 schema 保序、params 平铺。
        let req = TaggingRequest {
            prompt: "P",
            schema: &InlineSchema::ThreeInOne(three_in_one_schema()),
            model: "qwen3.5-4b",
            inputs: [TaggingInput { id: "1", image_url: "data:image/jpeg;base64,xx" }],
            params: TaggingParams { temperature: 0.1, max_tokens: 1024 },
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"params\":{\"temperature\":0.1,\"max_tokens\":1024}"), "wire: {s}");
        assert!(s.contains("\"required\":[\"title_ch\",\"title_en\",\"description\",\"tags\"]"), "wire: {s}");
    }

    #[test]
    fn test_connection_requires_configured() {
        let mut c = cfg();
        c.base_url = String::new();
        let p = DamaiTagProvider::new(spec(), c);
        let err = p.test_connection().unwrap_err();
        assert!(err.to_string().contains("未填写"), "got: {err}");
    }

    #[test]
    fn test_connection_unreachable_has_clear_error() {
        let p = DamaiTagProvider::new(spec(), cfg());
        let err = p.test_connection().unwrap_err();
        // 连接失败必须带 URL（验收：失败原因可读、可定位）
        assert!(err.to_string().contains("127.0.0.1:9"), "got: {err}");
    }

    #[test]
    fn tag_image_not_loaded_errors() {
        let p = DamaiTagProvider::new(spec(), cfg());
        let err = p.tag_image(std::path::Path::new("/nonexistent.jpg")).unwrap_err();
        assert!(matches!(err, TagError::NotLoaded(_)), "got: {err}");
    }

    #[test]
    fn inline_output_object_maps_to_value() {
        // 服务端 schema 校验后的 output 形态 → tag_inline 的归一化路径
        let out = serde_json::json!({ "value": "海边日落" });
        let v = out.get("value").and_then(|x| x.as_str()).unwrap_or_default();
        assert_eq!(parse_inline_value(v), "海边日落");
    }
}

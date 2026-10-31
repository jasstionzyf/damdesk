//! `OpenAiCompatTagProvider` — OpenAI 兼容 `chat/completions` 云 VL 打标（PLAN.md §9）。
//!
//! 「唯一可选增强」：设置了 endpoint + key + model 才启用；未配置时 `ready()=false`，
//! UI 显示「AI 打标未启用」引导，主流程（本地 ONNX 检索）不受影响。
//!
//! 请求形态（OpenAI 多模态 content parts）：
//! ```json
//! {"model":"vl","max_tokens":1024,"messages":[{"role":"user","content":[
//!   {"type":"text","text":"<模板>"},
//!   {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,..."}}]}]}
//! ```
//! 回复解析见 [`crate::parse_tagging_output`]（容忍 ```json 围栏与前后废话）。

use std::sync::atomic::{AtomicBool, Ordering};

use core_config::{ModelSpec, TagSettings};

use crate::http::image_data_url;
use crate::{Result, TagError, TagProvider, TaggingOutput};

/// OpenAI 兼容云 VL 打标 provider。
pub struct OpenAiCompatTagProvider {
    spec: ModelSpec,
    cfg: TagSettings,
    agent: ureq::Agent,
    loaded: AtomicBool,
}

impl OpenAiCompatTagProvider {
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

    fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
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
        // agent 已关 http_status_as_error：非 2xx 也走到这里，读完 body 再归类
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| TagError::Http(format!("read {url}: {e}")))?;
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            TagError::Http(format!(
                "bad json from {url} (status {status}): {e}; body={text}"
            ))
        })?;
        if !(200..300).contains(&status) {
            // 优先提取 OpenAI 风格 error.message，让 UI 能直接展示上游报错
            let msg = parsed
                .pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    parsed
                        .get("error")
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| text.clone())
                });
            return Err(TagError::Upstream(format!(
                "HTTP {status} from {url}: {msg}"
            )));
        }
        Ok(parsed)
    }

    /// 取 choices[0].message.content 并带上 finish_reason（截断判定要靠它）。
    fn chat_content(&self, body: &serde_json::Value) -> Result<(String, Option<String>)> {
        let resp = self.post_json("/chat/completions", body)?;
        if let Some(err) = resp.get("error") {
            return Err(TagError::Upstream(err.to_string()));
        }
        let choice = resp
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| TagError::Parse(format!("no choices[0] in {resp}")))?;
        let content = choice
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| TagError::Parse(format!("no choices[0].message.content in {resp}")))?;
        let finish = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(|s| s.to_string());
        Ok((content.to_string(), finish))
    }

    /// 解析回复并按「截断 / 真错」分类，错误文案里带上**本次实际下发的**
    /// max_tokens 与来源（默认 / 设置页覆盖），UI 失败明细才能直接指向预算问题。
    fn parse_or_tag_error(
        &self,
        content: &str,
        finish: Option<String>,
    ) -> Result<crate::TagParse> {
        use crate::TagParseError;
        let hit_length = finish.as_deref() == Some("length");
        match crate::parse_tagging_output_ex(content) {
            Ok(p) => Ok(p),
            Err(e) => {
                let detail = match e {
                    TagParseError::Truncated(d) => d,
                    // finish_reason=length 说明确实是预算砍断的（哪怕文本看起来不像 JSON）
                    TagParseError::Invalid(d) if !hit_length => return Err(TagError::Parse(d)),
                    TagParseError::Invalid(d) => d,
                };
                Err(TagError::Truncated {
                    max_tokens: self.cfg.effective_max_tokens(),
                    budget: self.cfg.max_tokens_label(),
                    detail: format!(
                        "finish_reason={}{}；{detail}",
                        finish.as_deref().unwrap_or("null"),
                        if hit_length {
                            "（输出被预算砍断，调大 max_tokens）"
                        } else {
                            ""
                        }
                    ),
                })
            }
        }
    }

    fn request_body(
        &self,
        prompt: &str,
        image_part: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let content = match image_part {
            Some(img) => serde_json::json!([
                { "type": "text", "text": prompt },
                img,
            ]),
            None => serde_json::json!(prompt),
        };
        serde_json::json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.effective_max_tokens(),
            "temperature": 0.1,
            "messages": [{ "role": "user", "content": content }],
        })
    }
}

impl TagProvider for OpenAiCompatTagProvider {
    fn load(&mut self) -> Result<()> {
        if !self.cfg.configured() {
            return Err(TagError::NotConfigured(
                "AI 打标未启用：请在设置里填写 OpenAI 兼容 endpoint 与模型名".to_string(),
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
        let img = serde_json::json!({
            "type": "image_url",
            "image_url": { "url": image_data_url(&bytes, &mime) }
        });
        let body = self.request_body(&self.cfg.prompt_template, Some(img));
        let (content, finish) = self.chat_content(&body)?;
        let mut parsed = self.parse_or_tag_error(&content, finish)?;
        if parsed.repaired {
            // 截断但补回来了：标签数可能少于模板要求，明确留痕（UI 报告里有 repaired 计数）
            eprintln!(
                "dam-tag: 输出被截断后补齐（max_tokens={}）{}",
                self.cfg.effective_max_tokens(),
                self.cfg.max_tokens_label()
            );
        }
        parsed.output.repaired = parsed.repaired;
        Ok(parsed.output)
    }

    fn test_connection(&self) -> Result<String> {
        if !self.cfg.configured() {
            return Err(TagError::NotConfigured("endpoint/模型名未填写".to_string()));
        }
        let body = self.request_body("连接测试：只回复 OK 两个字母。", None);
        let (content, _finish) = self.chat_content(&body)?;
        // 推理模型会把 <think>…</think> 混进 content，回显前剥离（UI 只见最终回复）
        Ok(crate::strip_thinking(&content))
    }

    /// D10 inline prompt 通道（旧单值通道，保留兜底）：输出约束模板包用户 prompt；
    /// max_tokens 收紧到 256（短文本约束），归一化交给 [`crate::parse_inline_value`]。
    fn tag_inline(&self, path: &std::path::Path, user_prompt: &str) -> Result<String> {
        if !self.ready() {
            return Err(TagError::NotLoaded(self.spec.name.clone()));
        }
        let (bytes, mime) = crate::vl_image::load_for_vl(path)?;
        let img = serde_json::json!({
            "type": "image_url",
            "image_url": { "url": image_data_url(&bytes, &mime) }
        });
        let mut body = self.request_body(&crate::build_inline_prompt(user_prompt), Some(img));
        if let Some(m) = body.get_mut("max_tokens") {
            *m = serde_json::json!(self.cfg.effective_max_tokens().min(256));
        }
        let (content, _finish) = self.chat_content(&body)?;
        Ok(crate::parse_inline_value(&content))
    }

    /// 统一标签通道（task）：prompt 直出 JSON 数组契约，回复过
    /// [`crate::parse_inline_values`]（数组解析 + 旧分隔符兜底）；无内容 → 空数组。
    fn tag_inline_values(&self, path: &std::path::Path, user_prompt: &str) -> Result<Vec<String>> {
        if !self.ready() {
            return Err(TagError::NotLoaded(self.spec.name.clone()));
        }
        let (bytes, mime) = crate::vl_image::load_for_vl(path)?;
        let img = serde_json::json!({
            "type": "image_url",
            "image_url": { "url": image_data_url(&bytes, &mime) }
        });
        let mut body = self.request_body(&crate::build_inline_prompt(user_prompt), Some(img));
        if let Some(m) = body.get_mut("max_tokens") {
            *m = serde_json::json!(self.cfg.effective_max_tokens().min(256));
        }
        let (content, _finish) = self.chat_content(&body)?;
        Ok(crate::parse_inline_values(&content))
    }

    fn ready(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    fn spec(&self) -> &ModelSpec {
        &self.spec
    }
}

//! L1 `engine-tag` — TagProvider trait；v1 仅 OpenAI 兼容云 VL（本地 tagger 留接口位）。
//! 打标模板：标题/描述/标签三合一（PLAN.md §6）。
//!
//! 分层：engine-tag 与 engine-embed 同层（互引禁止），故 HTTP agent/图片 data URL 等小工具
//! 在本 crate 内自持一份（`http` 模块），语义与 engine-embed::http 一致。

pub mod damai;
pub mod http;
pub mod openai_compat;
pub mod scan;
pub mod vl_image;

use core_config::{ModelSpec, TagSettings};

/// 打标结果：标题/描述/标签三合一（对齐 assets.title_ch/title_en/description/tags）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TaggingOutput {
    pub title_ch: String,
    pub title_en: String,
    pub description: String,
    pub tags: Vec<TagItem>,
    /// 输出被 max_tokens 截断、靠结构补齐救回（老数据/其他 provider 无此字段 → false）
    #[serde(default)]
    pub repaired: bool,
}

impl TaggingOutput {
    /// 三合一字段全空（截断补齐时用来判「补出来的东西没有价值」）。
    pub fn is_effectively_empty(&self) -> bool {
        self.title_ch.trim().is_empty()
            && self.title_en.trim().is_empty()
            && self.description.trim().is_empty()
            && self.tags.is_empty()
    }
}

/// 单个标签（origin 落库时固定 auto：自动打标不覆盖 manual）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagItem {
    pub name: String,
    #[serde(default = "default_score")]
    pub score: f32,
    #[serde(default = "default_origin")]
    pub origin: String,
}

fn default_score() -> f32 {
    0.9
}

fn default_origin() -> String {
    "auto".to_string()
}

impl TagItem {
    pub fn new(name: impl Into<String>, score: f32) -> Self {
        Self {
            name: name.into(),
            score,
            origin: "auto".to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("provider not loaded: {0}")]
    NotLoaded(String),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("not configured: {0}")]
    NotConfigured(String),
    #[error("http: {0}")]
    Http(String),
    #[error("response parse: {0}")]
    Parse(String),
    /// 模型输出被 max_tokens 截断（finish_reason=length / JSON 半截），且补齐救不回。
    /// `max_tokens`/`source` 是**本次实际下发**的预算与来源（默认 / 设置页覆盖）——
    /// 排障要能一眼看出「是预算给小了」而不是瞎猜（卡 task）。
    #[error("response truncated at max_tokens={max_tokens}（{budget}）: {detail}")]
    Truncated {
        max_tokens: u32,
        /// 预算来源人读标签（`2048（默认）` / `1024（设置页覆盖）`）；
        /// 不叫 source（thiserror 会把 source 字段当错误链）
        budget: String,
        detail: String,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, TagError>;

/// 打标 provider 契约（v1 只有 [`openai_compat::OpenAiCompatTagProvider`]）。
pub trait TagProvider: Send + Sync {
    fn load(&mut self) -> Result<()>;
    /// 单图打标（path 由调用方保证存在；调用方负责落库）。
    fn tag_image(&self, path: &std::path::Path) -> Result<TaggingOutput>;
    /// 连通性测试（设置页「测试连接」按钮）。
    fn test_connection(&self) -> Result<String>;
    fn ready(&self) -> bool;
    fn spec(&self) -> &ModelSpec;
    /// D10 inline prompt 通道（旧单值通道，保留兜底）：自定义打标任务用
    /// （用户 prompt + 输出约束模板，对齐 dam-ai /v1/tagging inline 设计）。
    /// 返回归一化后的标签值短文本；空串 = 图片无该维度内容。
    /// 默认不支持（本地 tagger 等未实现时显式报错）。
    fn tag_inline(&self, _path: &std::path::Path, _user_prompt: &str) -> Result<String> {
        Err(TagError::NotConfigured(
            "该 provider 不支持 inline prompt 通道".to_string(),
        ))
    }
    /// 统一标签通道（2026-10-19 裁决）：inline prompt → 标签值**数组**。
    /// 默认实现走旧单值通道 [`Self::tag_inline`] + [`parse_inline_values`]
    /// （JSON 数组与旧分隔符串都能吃）；provider 直出数组时覆盖本方法。
    fn tag_inline_values(
        &self,
        path: &std::path::Path,
        user_prompt: &str,
    ) -> Result<Vec<String>> {
        let s = self.tag_inline(path, user_prompt)?;
        Ok(parse_inline_values(&s))
    }
}

/// 三合一提示词模板默认值（可在设置页覆盖）。
pub const TAG_PROMPT_TEMPLATE: &str = core_config::DEFAULT_TAG_PROMPT;

// ---------- D10：inline prompt 通道（自定义打标任务） ----------

/// inline 标签值长度上限（超出截断；短文本是过滤维度可运营的前提）。
pub const INLINE_VALUE_MAX_CHARS: usize = 64;

/// inline 通道提示词 = 输出约束模板 + 用户 prompt（统一标签契约，2026-10-19 裁决：
/// 只输出 JSON 字符串数组；每元素一个离散标签、必须自解释、多词 `_` 连接；
/// 禁空格/顿号碎片；无内容输出 []。图内含什么由用户 prompt 描述，长度/形态约定固定在此）。
pub fn build_inline_prompt(user_prompt: &str) -> String {
    format!(
        "你是图片打标器。按用户要求分析图片并输出标签值数组。\n\
         【用户要求】{user_prompt}\n\
         【输出要求】只输出一个 JSON 字符串数组，例如 [\"MacBook_Pro_13寸\"]；\
         每个元素是一个离散标签：必须自解释，多词用「_」连接，\
         不超过 {INLINE_VALUE_MAX_CHARS} 字；禁空格、顿号、解释文字；\
         不要输出数组以外的任何内容；\
         如果图片中不存在该内容或无法判断，只输出 []。"
    )
}

/// 从文本中抠出**第一个能被 serde 解析的括号配平 JSON 对象**切片（字符串感知）。
///
/// 背景（journey UI 实测 qwen3.5-4b）：模型偶尔在 JSON 前后夹无标签的闲话，
/// 且闲话里可能再出现 `{`/`}`。旧的 `find('{')..rfind('}')` 区间会被前导闲话
/// 里的 `{` 或尾随解释里的 `}` 拉宽，serde 报 trailing characters / expected value。
/// 这里逐个候选做带字符串转义状态的深度配平并试解析：闲话里凑巧配平的 `{..}`
/// （非法 JSON）会被跳过，取第一个真正合法的完整对象；找不到（真截断）返回
/// None，交由上层截断补齐逻辑。
pub fn first_balanced_json(s: &str) -> Option<&str> {
    let mut start = s.find('{')?;
    loop {
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escaped = false;
        let mut end = None;
        for (i, ch) in s[start..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' if in_str => escaped = true,
                '"' => in_str = !in_str,
                '{' if !in_str => depth += 1,
                '}' if !in_str => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(start + i + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }
        let e = end?;
        let cand = &s[start..e];
        if serde_json::from_str::<serde_json::Value>(cand).is_ok() {
            return Some(cand);
        }
        // 该配平段不是合法 JSON（前导闲话里凑巧配平的 {..}）：从其后找下一个 '{'
        match s[e..].find('{') {
            Some(rel) => start = e + rel,
            None => return None,
        }
    }
}

/// inline 回复归一化（旧单值通道）：剥 ``` 围栏 → 容忍 {"value": "..."} JSON → trim →
/// 「无内容」约定归空串 → 超长截断（按 char 边界）。
pub fn parse_inline_value(text: &str) -> String {
    let t = strip_code_fence(text);
    // JSON 形态：取 value 字段（模型没听话直接输出短文本时走原文本）
    let mut s = t.clone();
    if let (Some(start), Some(end)) = (t.find('{'), t.rfind('}')) {
        if end > start {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t[start..=end]) {
                if let Some(val) = v.get("value").and_then(|x| x.as_str()) {
                    s = val.to_string();
                }
            }
        }
    }
    let s = s
        .trim()
        .trim_matches(|c: char| c == '"' || c == '。' || c == '.');
    if s.is_empty() || s == "无" || s == "无内容" || s == "无法判断" || s == "不适用" {
        return String::new();
    }
    if s.chars().count() > INLINE_VALUE_MAX_CHARS {
        s.chars().take(INLINE_VALUE_MAX_CHARS).collect()
    } else {
        s.to_string()
    }
}

/// inline 数组解析（统一标签主路径）：剥围栏后先试 JSON 字符串数组（元素容忍
/// {value|name} 对象形态），解不出数组再退回旧形态——`{"value": "a; b"}` 或纯文本
/// 按 `;` `；` 换行拆分；每值过 [`normalize_inline_value`]、去重、保序。
/// 返回空 = 无内容（含 `[]` 约定）。
pub fn parse_inline_values(text: &str) -> Vec<String> {
    let t = strip_code_fence(text);
    // 「无内容」约定（旧单值通道措辞兼容）
    let bare = t.trim();
    if bare.is_empty() || bare == "无" || bare == "无内容" || bare == "无法判断" || bare == "不适用"
    {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: &str, out: &mut Vec<String>| {
        if let Some(n) = normalize_inline_value(v) {
            if !out.iter().any(|x| *x == n) {
                out.push(n);
            }
        }
    };
    // 1) JSON 数组形态（统一标签契约）
    if let (Some(start), Some(end)) = (t.find('['), t.rfind(']')) {
        if end > start {
            if let Ok(serde_json::Value::Array(arr)) = serde_json::from_str(&t[start..=end]) {
                for item in arr {
                    match &item {
                        serde_json::Value::String(s) => push(s, &mut out),
                        serde_json::Value::Object(o) => {
                            if let Some(v) = o
                                .get("value")
                                .or_else(|| o.get("name"))
                                .and_then(|x| x.as_str())
                            {
                                push(v, &mut out);
                            }
                        }
                        _ => {}
                    }
                }
                // 数组解析成功即返回（空数组 = 无内容，语义就是「没有该维度」）
                return out;
            }
        }
    }
    // 2) 旧对象形态：{"value": "..."}（值里可能是分号串）。对象能解析出 value
    //    字符串就按旧通道收尾——value 归空（如 damai schema 的空回）= 无内容，
    //    不许落进第 3 步把 JSON 文本本身拆成碎片。
    if let (Some(start), Some(end)) = (t.find('{'), t.rfind('}')) {
        if end > start {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t[start..=end]) {
                if let Some(val) = v.get("value").and_then(|x| x.as_str()) {
                    for piece in val.split([';', '；', '\n', '\r']) {
                        push(piece, &mut out);
                    }
                    return out;
                }
            }
        }
    }
    // 3) 兜底：纯文本按旧分隔符拆分（老 provider / 模型没按数组输出）
    for piece in t.split([';', '；', '\n', '\r']) {
        push(piece, &mut out);
    }
    out
}

/// 值归一化：与 core_storage::tags::normalize_tag_value 同规则（engine 层禁依赖
/// storage，各持一份同口径实现 + 同口径测试；落库侧 fanout_values 会再归一一次，
/// 幂等）。trim、去引号、去句末标点、内部空白折叠成 `_`、去前后 `_`、截断、空→None。
fn normalize_inline_value(raw: &str) -> Option<String> {
    let mut s = raw.trim().trim_matches('"').trim();
    loop {
        let trimmed = s.trim_end_matches(['。', '.', '！', '!', '？', '?', '；', ';', '，', ',']);
        if trimmed.len() == s.len() {
            break;
        }
        s = trimmed.trim_end();
    }
    let mut out = String::with_capacity(s.len());
    let mut pending_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            pending_ws = true;
            continue;
        }
        if pending_ws && !out.is_empty() {
            out.push('_');
        }
        pending_ws = false;
        out.push(ch);
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        return None;
    }
    let out: String = out.chars().take(INLINE_VALUE_MAX_CHARS).collect();
    (!out.is_empty()).then_some(out)
}

/// 从模型回复里抠出 JSON 并解析（容忍 ```json 围栏/前后废话）。
/// 截断输出会先尝试结构补齐（见 [`parse_tagging_output_ex`]），救不回来才报错。
pub fn parse_tagging_output(text: &str) -> Result<TaggingOutput> {
    parse_tagging_output_ex(text)
        .map(|p| p.output)
        .map_err(|e| match e {
            TagParseError::Truncated(d) => TagError::Parse(format!("truncated: {d}")),
            TagParseError::Invalid(d) => TagError::Parse(d),
        })
}

/// 解析结果：三合一输出 + 是否靠「截断补齐」救回。
#[derive(Debug, Clone)]
pub struct TagParse {
    pub output: TaggingOutput,
    /// 模型输出被 max_tokens 截断，靠补齐未闭合的 `]`/`}` 解析成功。
    /// 标签可能少于模板要求的 5~10 个 → UI 提示「把 max_tokens 调大」。
    pub repaired: bool,
}

/// 解析失败分类：provider 据此把「预算截断」和「真·非法 JSON」分开报。
#[derive(Debug, Clone, PartialEq)]
pub enum TagParseError {
    /// 输出中途断了（max_tokens / finish_reason=length）且补齐救不回来
    Truncated(String),
    /// 非截断类非法输出（压根不是 JSON / 结构不对）
    Invalid(String),
}

/// 解析 + 区分截断：先按完整 JSON 解析；失败再尝试「补齐未闭合 `]`/`}`」；
/// 仍失败则按 serde 的 EOF 特征归类为 truncation 或真错。
///
/// 截断修复只补结构、不猜内容：候选切点取「完整 JSON token 结尾」，从后往前
/// 试，首个能通过 serde 校验的组合才采用。补出来的结果若三合一字段全空则放弃
/// （宁可按失败报出来，也不落一条空标签）。
pub fn parse_tagging_output_ex(text: &str) -> std::result::Result<TagParse, TagParseError> {
    let cleaned = strip_code_fence(text);
    let Some(start) = cleaned.find('{') else {
        return Err(TagParseError::Invalid(format!("no json object in: {text}")));
    };
    let slice = &cleaned[start..];

    // 1) 完整 JSON（容忍尾随废话）。先做括号配平扫描：前导/尾随闲话里再出现
    //    `{`/`}` 也能精确抠出第一个完整对象（实测 qwen3.5-4b 无标签闲话形态）。
    if let Some(bal) = first_balanced_json(slice) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(bal) {
            return Ok(TagParse {
                output: from_json(&v),
                repaired: false,
            });
        }
    }
    let intact_err = if let Some(end) = slice.rfind('}') {
        match serde_json::from_str::<serde_json::Value>(&slice[..=end]) {
            Ok(v) => {
                return Ok(TagParse {
                    output: from_json(&v),
                    repaired: false,
                })
            }
            Err(e) => Some(e),
        }
    } else {
        None
    };

    // 2) 截断补齐（只补括号，不补内容）
    if let Some(fixed) = repair_truncated_json(slice) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&fixed) {
            let output = from_json(&v);
            if !output.is_effectively_empty() {
                return Ok(TagParse {
                    output,
                    repaired: true,
                });
            }
        }
    }

    // 3) 归类：serde 明确报 EOF（输入提前结束）→ 截断；本次连 `}` 都没有 → 截断
    let truncated = match &intact_err {
        Some(e) => e.is_eof(),
        None => true,
    };
    let detail = match &intact_err {
        Some(e) => format!("json: {e}; raw={}", tail_snippet(slice, 600)),
        None => format!("no json end (output cut off); raw={}", tail_snippet(slice, 600)),
    };
    Err(if truncated {
        TagParseError::Truncated(detail)
    } else {
        TagParseError::Invalid(detail)
    })
}

/// 取片段尾部（截断错误里前文通常完好，尾部才是断点）。
fn tail_snippet(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let skip = n - max_chars;
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

/// 截断补齐：把模型输出当成「写了一半的 JSON」，找最长的可闭合前缀。
///
/// 实现：扫一遍记录候选切点（字符串闭合处 / `}` / `]` / 数字结尾），从后往前
/// 逐个「切到该处 + 按栈逆序补 `]`/`}`」，交给 serde 校验，首个通过即返回。
/// 只补结构不猜内容 → 截断点所在的半个 tag 被丢弃，已完整的部分保留。
fn repair_truncated_json(slice: &str) -> Option<String> {
    let mut cuts: Vec<usize> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    let mut prev_digit = false;
    for (i, ch) in slice.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
                cuts.push(i + ch.len_utf8());
            }
            prev_digit = false;
            continue;
        }
        match ch {
            '"' => in_str = true,
            '}' | ']' => cuts.push(i + ch.len_utf8()),
            '0'..='9' => prev_digit = true,
            _ => {
                if prev_digit {
                    cuts.push(i);
                }
                prev_digit = false;
            }
        }
    }
    if prev_digit {
        cuts.push(slice.len());
    }
    // 候选太多时只试尾部（截断点一定在末尾附近），保持 O(常数) 次解析
    let tail = cuts.len().saturating_sub(400);
    for cut in cuts[tail..].iter().rev() {
        let mut head = slice[..*cut].trim_end().to_string();
        // 切点后面若剩下悬空的分隔符/键，一并去掉
        while head.ends_with(',') || head.ends_with(':') {
            head.pop();
            head = head.trim_end().to_string();
        }
        let mut closers = String::new();
        let mut stack: Vec<char> = Vec::new();
        let mut in_str2 = false;
        let mut esc2 = false;
        for ch in head.chars() {
            if in_str2 {
                if esc2 {
                    esc2 = false;
                } else if ch == '\\' {
                    esc2 = true;
                } else if ch == '"' {
                    in_str2 = false;
                }
                continue;
            }
            match ch {
                '"' => in_str2 = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' => {
                    stack.pop();
                }
                _ => {}
            }
        }
        if in_str2 {
            continue; // 切点落在字符串内部，结构不可补
        }
        while let Some(c) = stack.pop() {
            closers.push(c);
        }
        if closers.is_empty() {
            continue;
        }
        let candidate = format!("{head}{closers}");
        if serde_json::from_str::<serde_json::Value>(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
}


/// 结构化解析：tags 支持 [{"name","score"}] 与 ["纯字符串"] 两种写法。
fn from_json(v: &serde_json::Value) -> TaggingOutput {
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let mut tags = Vec::new();
    if let Some(arr) = v.get("tags").and_then(|t| t.as_array()) {
        for item in arr {
            match item {
                serde_json::Value::String(name) => {
                    if !name.trim().is_empty() {
                        tags.push(TagItem::new(name.trim(), default_score()));
                    }
                }
                serde_json::Value::Object(o) => {
                    let name = o
                        .get("name")
                        .or_else(|| o.get("tag"))
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let score = o
                        .get("score")
                        .and_then(|x| x.as_f64())
                        .map(|f| f as f32)
                        .unwrap_or_else(default_score);
                    let origin = o
                        .get("origin")
                        .and_then(|x| x.as_str())
                        .unwrap_or("auto")
                        .to_string();
                    tags.push(TagItem {
                        name,
                        score,
                        origin,
                    });
                }
                _ => {}
            }
        }
    }
    TaggingOutput {
        title_ch: s("title_ch"),
        title_en: s("title_en"),
        description: s("description"),
        tags,
        repaired: false,
    }
}

/// 已是 JSON 对象的模型输出 → TaggingOutput（damai provider 的
/// /v1/tagging output 形态：guided decoding 后服务端已按 schema 校验）。
pub fn tagging_output_from_value(v: &serde_json::Value) -> TaggingOutput {
    from_json(v)
}

/// R7 provider 装配单一入口：按 `TagSettings.provider` 选实现。
/// dam-ai /v1/tagging（自托管默认）→ [`damai::DamaiTagProvider`]；
/// dashscope / zhipu / ollama / custom（OpenAI 兼容 /v1）→
/// [`openai_compat::OpenAiCompatTagProvider`]。未知 id 一律走 openai_compat
/// （与 D9「未知/老配置 → custom 兜底」同语义）。
pub fn build_provider(spec: ModelSpec, cfg: TagSettings) -> Box<dyn TagProvider> {
    if cfg.provider == core_config::TAG_PROVIDER_DAMAI {
        Box::new(damai::DamaiTagProvider::new(spec, cfg))
    } else {
        Box::new(openai_compat::OpenAiCompatTagProvider::new(spec, cfg))
    }
}

/// 去掉 ```json ... ``` 围栏。
fn strip_code_fence(text: &str) -> String {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

/// 剥离推理模型混在 content 里的思考段（大小写/多行容忍）。用于「测试连接」
/// 回显：成功文案只留模型最终回复，思考链不出现在 UI。三种形态：
/// 1. 成对 `<think>…</think>`（可多段）→ 剥离对内文本；
/// 2. 只有闭合 `</think>`（qwen3.5-4b 无 reasoning parser 实测：思考文本
///    直接开头、仅以 `</think>` 收尾）→ 剥离闭合标签之前的整段；
/// 3. 只有未闭合 `<think>`（截断输出）→ 之后整段按思考处理。
/// reasoning_content 类字段本就不读取（chat_content 只取 message.content）。
pub fn strip_thinking(content: &str) -> String {
    let lower = content.to_lowercase();
    let mut out = String::with_capacity(content.len());
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find("<think>") {
        let open = pos + rel;
        out.push_str(&content[pos..open]);
        match lower[open..].find("</think>") {
            Some(c) => pos = open + c + "</think>".len(),
            // 未闭合：之后全是思考段
            None => return out.trim().to_string(),
        }
    }
    out.push_str(&content[pos..]);
    // 孤立闭合标签（成对剥离后剩余部分仍含 </think>）：之前整段按思考处理
    if let Some(rel) = lower[pos..].rfind("</think>") {
        return content[pos + rel + "</think>".len()..].trim().to_string();
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_balanced_json_picks_object_amid_prose() {
        // 前导闲话含 '{'、尾随解释含 '}'：必须精确抠出第一个完整对象
        let s = "The user wants me to tag {this image} so here:\n{\"title_ch\": \"手提包\", \"tags\": []}\nExplanation: object closed at } end.";
        assert_eq!(
            first_balanced_json(s),
            Some("{\"title_ch\": \"手提包\", \"tags\": []}")
        );
        // 字符串字面量里的 '{'/'}' 不计深度
        let s2 = "{\"desc\": \"含 } 与 { 的正文\", \"n\": 1}";
        assert_eq!(first_balanced_json(s2), Some(s2));
        // 转义引号不翻转字符串状态
        let s3 = r#"{"desc": "他说：\"}{\"", "n": 2}"#;
        assert_eq!(first_balanced_json(s3), Some(s3));
        // 真截断（无配平）→ None
        assert_eq!(first_balanced_json("{\"a\": [1, 2"), None);
        // 无对象 → None
        assert_eq!(first_balanced_json("没有 JSON"), None);
    }

    #[test]
    fn template_mentions_all_fields() {
        let t = TAG_PROMPT_TEMPLATE;
        for k in ["title_ch", "title_en", "description", "tags"] {
            assert!(t.contains(k), "missing {k}");
        }
    }

    #[test]
    fn strip_thinking_removes_reasoning_chain() {
        // 普通回复原样（trim）
        assert_eq!(strip_thinking("OK"), "OK");
        // 思考段在前，最终回复在后（qwen3.5-4b 实测形态）
        assert_eq!(strip_thinking("<think>\n分析：用户要 OK。\n</think>\nOK"), "OK");
        // 多行 + 大写标签
        assert_eq!(
            strip_thinking("<THINK>long\nreasoning</THINK>final answer"),
            "final answer"
        );
        // 未闭合：之后整段按思考处理
        assert_eq!(strip_thinking("<think>truncated"), "");
        // 孤立闭合 </think>（qwen3.5-4b 实测：无 reasoning parser，思考正文
        // 直接开头，仅以 </think> 收尾）→ 之前整段剥离
        assert_eq!(strip_thinking("Thinking Process:\n1. analyze\n</think>\n\nOK"), "OK");
        // 无标签的普通多行回复不受影响
        assert_eq!(strip_thinking("line1\nline2"), "line1\nline2");
    }

    #[test]
    fn build_provider_dispatches_by_preset() {
        use core_config::TagSettings;
        fn spec() -> ModelSpec {
            ModelSpec {
                name: "openai-compat-tagger".into(),
                kind: core_config::SpecKind::Tagging,
                dims: 0,
                modalities: vec!["image".into()],
                normalized: false,
                provider: "openai_compat".into(),
                preprocess: Default::default(),
            }
        }
        let mut cfg = TagSettings::default();
        cfg.provider = core_config::TAG_PROVIDER_DAMAI.into();
        let p = build_provider(spec(), cfg);
        assert_eq!(p.spec().name, "openai-compat-tagger");
        // 非 damai（含未知/老配置）一律 openai_compat
        let mut cfg2 = TagSettings::default();
        cfg2.provider = "unknown-legacy".into();
        let _p2 = build_provider(spec(), cfg2);
        let p3 = build_provider(
            spec(),
            TagSettings {
                provider: core_config::TAG_PROVIDER_DASHSCOPE.into(),
                ..Default::default()
            },
        );
        assert_eq!(p3.spec().name, "openai-compat-tagger");
    }

    #[test]
    fn tagging_output_from_value_maps_object() {
        let v = serde_json::json!({
            "title_ch": "金毛犬", "title_en": "Golden retriever",
            "description": "草地上", "tags": ["狗", "草地"]
        });
        let out = tagging_output_from_value(&v);
        assert_eq!(out.title_ch, "金毛犬");
        assert_eq!(out.tags.len(), 2);
    }

    #[test]
    fn parses_fenced_and_bare_json() {
        let bare = r#"{"title_ch":"金毛犬","title_en":"Golden retriever","description":"一只坐在草地上的金毛","tags":[{"name":"狗","score":0.98},{"name":"草地","score":0.7}]}"#;
        let out = parse_tagging_output(bare).unwrap();
        assert_eq!(out.title_ch, "金毛犬");
        assert_eq!(out.tags.len(), 2);
        assert_eq!(out.tags[0].name, "狗");
        assert!((out.tags[0].score - 0.98).abs() < 1e-6);
        assert_eq!(out.tags[0].origin, "auto");

        let fenced = format!("```json\n{bare}\n```");
        assert_eq!(parse_tagging_output(&fenced).unwrap().title_ch, "金毛犬");

        let chatty = format!("好的，这是结果：\n{bare}\n希望有帮助！");
        assert_eq!(parse_tagging_output(&chatty).unwrap().tags.len(), 2);
    }

    #[test]
    fn tolerates_string_tags_and_defaults() {
        let out = parse_tagging_output(r#"{"title_ch":"猫","tags":["猫","宠物",{"tag":"室内"}]}"#)
            .unwrap();
        assert_eq!(out.title_en, "");
        assert_eq!(out.tags.len(), 3);
        assert_eq!(out.tags[2].name, "室内");
        assert_eq!(out.tags[0].score, 0.9);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_tagging_output("完全不是 JSON").is_err());
        assert!(parse_tagging_output("{broken json}").is_err());
    }

    // ---------- max_tokens 截断（卡 task） ----------

    /// 真机原文（WIN-E2E steps.jsonl tag_status_final，资产7）：输出在 tags 数组
    /// 第 6 个元素后被 1024 砍断。修复要求：补齐结构救回前半段，不能整张判失败。
    const TRUNCATED_EVIDENCE: &str = r#"{
  "title_ch": "金黄炸薯条配番茄酱",
  "title_en": "Golden French Fries with Ketchup",
  "description": "木制砧板上金黄色的厚切炸薯条，配有红艳的番茄酱和新鲜迷迭香点缀，诱人食欲。",
  "tags": [
    {
      "name": "炸薯条",
      "score": 0.99
    },
    {
      "name": "番茄酱",
      "score": 0.98
    },
    {
      "name": "木砧板",
      "score": 0.95
    },
    {
      "name": "迷迭香",
      "score": 0.92
    },
    {
      "name": "美食摄影",
      "score": 0.9
    },
    {
      "name": "零食",
      "score": 0.88
    }"#;

    #[test]
    fn truncated_output_is_repaired_from_real_evidence() {
        let parsed = parse_tagging_output_ex(TRUNCATED_EVIDENCE).expect("截断应能补齐");
        assert!(parsed.repaired, "必须标记为补齐过的");
        assert_eq!(parsed.output.title_ch, "金黄炸薯条配番茄酱");
        assert_eq!(parsed.output.tags.len(), 6, "已完整的 6 个标签要保留");
        assert_eq!(parsed.output.tags[5].name, "零食");
        // 兼容旧入口：parse_tagging_output 直接拿到结果（不再报 EOF）
        let legacy = parse_tagging_output(TRUNCATED_EVIDENCE).unwrap();
        assert_eq!(legacy.tags.len(), 6);
    }

    #[test]
    fn truncated_mid_tag_drops_the_half_item_and_keeps_the_rest() {
        // 断在第 3 个 tag 的键名中间（真机另一种形态：半截对象）
        let cut = r#"{"title_ch":"猫","description":"窗台上的猫","tags":[{"name":"猫","score":0.9},{"name":"室内","score":0.8},{"na"#;
        let parsed = parse_tagging_output_ex(cut).expect("半截 tag 应能补齐");
        assert!(parsed.repaired);
        assert_eq!(parsed.output.tags.len(), 2, "半截 tag 丢弃，完整的前两个保留");
        assert_eq!(parsed.output.title_ch, "猫");
        assert_eq!(parsed.output.description, "窗台上的猫");

        // 断在字符串值中间
        let cut2 = r#"{"title_ch":"金毛犬","description":"一只趴在草地上的金毛"#;
        let parsed2 = parse_tagging_output_ex(cut2).expect("半截字符串应能补齐");
        assert!(parsed2.repaired);
        assert_eq!(parsed2.output.title_ch, "金毛犬");
        assert!(
            parsed2.output.description.is_empty(),
            "被砍断的 description 不落半句话"
        );
    }

    #[test]
    fn unrepairable_and_real_errors_are_classified() {
        // 只有开头 → 补不出任何有效内容 → 截断类失败（可区分于真错）
        let empty_head = parse_tagging_output_ex(r#"{"title_ch": "#);
        assert!(
            matches!(empty_head, Err(TagParseError::Truncated(_))),
            "got {empty_head:?}"
        );
        // 空 content（推理模型把预算全花在思考上，finish_reason=length 的常见形态）
        assert!(matches!(
            parse_tagging_output_ex(""),
            Err(TagParseError::Invalid(_))
        ));
        // 完整但不是 JSON → 真错（不许被当成截断吞掉）
        assert!(matches!(
            parse_tagging_output_ex("{broken json}"),
            Err(TagParseError::Invalid(_))
        ));
        // 结构错（tags 不是数组）→ 解析成功但无标签，仍算 invalid 侧的完整 JSON
        let weird = parse_tagging_output_ex(r#"{"title_ch":"猫","tags":"不是数组"}"#).unwrap();
        assert_eq!(weird.output.title_ch, "猫");
        assert!(weird.output.tags.is_empty());
    }

    #[test]
    fn default_max_tokens_covers_chinese_three_in_one_output() {
        use core_config::TagSettings;
        let cfg = TagSettings::default();
        assert_eq!(cfg.effective_max_tokens(), TagSettings::DEFAULT_MAX_TOKENS);
        assert_eq!(cfg.max_tokens_source(), "default");
        assert_eq!(cfg.max_tokens_label(), format!("{}（默认）", TagSettings::DEFAULT_MAX_TOKENS));
        // 0 = 未设置 → 默认值；显式改小 → 标记为设置页覆盖
        let zero = TagSettings {
            max_tokens: 0,
            ..Default::default()
        };
        assert_eq!(zero.effective_max_tokens(), TagSettings::DEFAULT_MAX_TOKENS);
        assert_eq!(zero.max_tokens_source(), "default");
        let small = TagSettings {
            max_tokens: 1024,
            ..Default::default()
        };
        assert_eq!(small.max_tokens_label(), "1024（设置页覆盖）");
    }

    // ---------- D10 inline 通道 ----------

    #[test]
    fn inline_prompt_carries_user_prompt_and_constraints() {
        let p = build_inline_prompt("这张照片的拍摄地点是什么？");
        assert!(p.contains("这张照片的拍摄地点是什么？"));
        // 统一标签契约：只输出 JSON 字符串数组、值自解释、多词 _ 连接、无内容 []
        for kw in ["用户要求", "输出要求", "JSON", "数组", "_", "[]"] {
            assert!(p.contains(kw), "missing {kw}");
        }
        assert!(p.contains(&INLINE_VALUE_MAX_CHARS.to_string()));
    }

    #[test]
    fn inline_value_normalization() {
        // 纯文本 / 围栏 / JSON value / 引号包裹都归一
        assert_eq!(parse_inline_value("海边日落"), "海边日落");
        assert_eq!(parse_inline_value("  海边日落。 "), "海边日落");
        assert_eq!(
            parse_inline_value("```json\n{\"value\":\"山顶\"}\n```"),
            "山顶"
        );
        assert_eq!(
            parse_inline_value("{\"value\": \"城市夜景; 蓝调\"}"),
            "城市夜景; 蓝调"
        );
        assert_eq!(parse_inline_value("\"暖橙\""), "暖橙");
        // 空内容约定 → 空串
        for e in ["", "  ", "无", "无法判断", "无内容", "不适用"] {
            assert_eq!(parse_inline_value(e), "", "case {e:?}");
        }
        // 超长截断（char 边界，不切多字节）
        let long = "长".repeat(INLINE_VALUE_MAX_CHARS + 10);
        let out = parse_inline_value(&long);
        assert_eq!(out.chars().count(), INLINE_VALUE_MAX_CHARS);
    }

    // ---------- 统一标签：数组解析（task） ----------

    #[test]
    fn parse_inline_values_eats_json_array_with_normalization() {
        // 契约形态：JSON 字符串数组；值带空白/句末标点 → 归一；重复 → 去重
        assert_eq!(
            parse_inline_values(r#"["MacBook Pro 13寸", "海尔 冰箱 501L。", "MacBook   Pro 13寸"]"#),
            vec![
                "MacBook_Pro_13寸".to_string(),
                "海尔_冰箱_501L".to_string()
            ]
        );
        // 围栏包裹 + 前后废话都能抠出数组
        assert_eq!(
            parse_inline_values("```json\n[\"暖橙\", \"冷蓝\"]\n```\n以上是结果"),
            vec!["暖橙".to_string(), "冷蓝".to_string()]
        );
        // 元素是 {value}/{name} 对象形态也吃
        assert_eq!(
            parse_inline_values(r#"[{"value":"青岛"}, {"name":"海 边"}]"#),
            vec!["青岛".to_string(), "海_边".to_string()]
        );
        // 空数组 / 无内容约定 → 空（= 该维度无内容）
        assert!(parse_inline_values("[]").is_empty());
        for e in ["", "  ", "无", "无法判断"] {
            assert!(parse_inline_values(e).is_empty(), "case {e:?}");
        }
    }

    #[test]
    fn parse_inline_values_falls_back_to_legacy_forms() {
        // 旧对象形态：value 里是分号串（damai ShortValue schema 的真实输出）
        assert_eq!(
            parse_inline_values("{\"value\": \"城市夜景; 蓝调\"}"),
            vec!["城市夜景".to_string(), "蓝调".to_string()]
        );
        // 纯文本兜底：分号/换行拆分
        assert_eq!(
            parse_inline_values("海边日落; 沙滩\n山顶。"),
            vec!["海边日落".to_string(), "沙滩".to_string(), "山顶".to_string()]
        );
        // 单值纯文本（旧 tag_inline 通道的常规返回）
        assert_eq!(
            parse_inline_values("\"暖橙\""),
            vec!["暖橙".to_string()]
        );
        // 全是无效值 → 空
        assert!(parse_inline_values("  ;;  ").is_empty());
        // 旧通道的空回（damai schema {"value": ""}）→ 空，不把 JSON 文本拆成碎片
        assert!(parse_inline_values("{\"value\": \"\"}").is_empty());
        assert!(parse_inline_values("{\"value\": \"  \"}").is_empty());
    }
}

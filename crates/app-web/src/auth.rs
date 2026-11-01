//! W7：Tauri 内嵌场景的鉴权（一次性 token 引导 + 会话 cookie）。
//!
//! 桌面壳启动时随机生成 token，内嵌 server 只绑 127.0.0.1 随机端口；窗口导航到
//! `/auth/boot?token=..&next=/` 完成一次性交换（token 用后即焚），之后整个会话靠
//! HttpOnly cookie（`dam_session`）通行，同源 fetch / `<img>` 自动携带，前端 bundle
//! 零改动（web/desktop 一套代码，R3/R4）。
//!
//! - token 未配置（web 容器 / dev vite 反代）→ 鉴权关闭，全部放行（R4 v1 单用户不变）。
//! - `/healthz` 与 `/auth/*` 豁免（容器探活 / 引导入口）。
//! - 会话只存内存，进程重启即失效（重新引导即可，无需持久化）。

use axum::extract::{Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::sync::Mutex;

use crate::AppState;

/// 会话 cookie 名（HttpOnly；由 /auth/boot 下发）。
const SESSION_COOKIE: &str = "dam_session";

/// 生成 32 位 hex 随机串（token / session 共用；CSPRNG = OS 熵源）。
pub fn generate_token() -> String {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).expect("OS entropy unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 鉴权状态：一次性 token（Some=启用，用后即焚）+ 进程内会话 id。
pub struct Auth {
    token: Mutex<Option<String>>,
    session: Mutex<Option<String>>,
}

impl Auth {
    pub fn new(token: Option<String>) -> Self {
        Self {
            token: Mutex::new(token),
            session: Mutex::new(None),
        }
    }

    /// 鉴权是否启用（token 已签发过会话或仍在 = 启用）。
    pub fn enabled(&self) -> bool {
        self.token.lock().unwrap().is_some() || self.session.lock().unwrap().is_some()
    }

    /// 一次性交换：token 匹配且未用过 → 签发会话，返回 true。token 用后即焚。
    fn exchange(&self, token: &str) -> bool {
        let mut t = self.token.lock().unwrap();
        match t.as_deref() {
            Some(expected) if constant_eq(expected, token) => {
                *self.session.lock().unwrap() = Some(generate_token());
                *t = None; // 一次性：burn
                true
            }
            _ => false,
        }
    }

    fn session_valid(&self, presented: Option<&str>) -> bool {
        match (self.session.lock().unwrap().as_deref(), presented) {
            (Some(s), Some(p)) => constant_eq(s, p),
            _ => false,
        }
    }

    /// 进程内重新签发一次性引导 token（R37）。
    ///
    /// 为什么需要：桌面壳启动时的 token 在 `/auth/boot` 交换后即焚毁；一旦 WebView2 浏览器
    /// 进程被杀，壳必须重建 webview，而重建后的窗口仍要经过一次 `/auth/boot` 交换——此时
    /// 旧 token 已失效，只能重新签发。
    ///
    /// 安全边界不变：新 token **不经网络暴露**（只由持有 `Arc<Auth>` 的壳进程内取用），仍然
    /// 一次性（下次 boot 交换后即焚），会话仍是 HttpOnly cookie。签发会让上一条会话失效，
    /// 桌面单窗口场景下等价（旧 webview 已经死了）。
    pub fn reissue(&self) -> String {
        let fresh = generate_token();
        *self.token.lock().unwrap() = Some(fresh.clone());
        fresh
    }
}

/// 常数时间比较（防时序侧信道；本地场景属纵深防御，成本为零）。
fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// 从 Cookie 头提取会话值（手写扫描，避免引 parser 依赖）。
fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(rest) = pair.strip_prefix(name) {
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// 全局中间件：豁免 /healthz 与 /auth/*；其余在鉴权启用时要求有效会话。
/// 未鉴权访问一律 401（引导链接只发一次，重复打开无效，需从应用窗口进入）。
pub async fn require_session(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let auth = &state.auth;
    let path = req.uri().path();
    let exempt = path == "/healthz" || path.starts_with("/auth/");
    if exempt || !auth.enabled() {
        return next.run(req).await;
    }
    if auth.session_valid(cookie_value(req.headers(), SESSION_COOKIE).as_deref()) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        "unauthorized: 请从应用窗口进入（一次性引导链接已失效时重启应用）",
    )
        .into_response()
}

/// GET /auth/boot?token=..&next=..：一次性 token 换会话 cookie，302 进应用。
#[derive(Deserialize)]
pub struct BootQuery {
    pub token: Option<String>,
    pub next: Option<String>,
}

pub async fn boot(State(state): State<AppState>, Query(q): Query<BootQuery>) -> Response {
    // next 只允许站内相对路径（防开放重定向）
    let next = match q.next.as_deref() {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => n.to_string(),
        _ => "/".to_string(),
    };
    let token = q.token.unwrap_or_default();
    if !state.auth.exchange(&token) {
        return (
            StatusCode::FORBIDDEN,
            "引导链接已失效（token 一次性）：请从桌面应用重新启动进入",
        )
            .into_response();
    }
    let session = state
        .auth
        .session
        .lock()
        .unwrap()
        .clone()
        .expect("session just issued");
    let mut res = Redirect::to(&next).into_response();
    // HttpOnly：JS 不可读；SameSite=Lax：同源 fetch/img 自动携带
    res.headers_mut().insert(
        HeaderName::from_static("set-cookie"),
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400"
        ))
        .expect("cookie header"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R37：重建窗口腿 —— 重新签发后新 token 可交换，旧 token 失效，会话仍有效。
    #[test]
    fn reissue_supersedes_old_token_and_keeps_guard_on() {
        let auth = Auth::new(Some("first".into()));
        assert!(auth.enabled());
        // 启动引导：旧 token 交换成功并焚毁
        assert!(auth.exchange("first"));
        let session = auth.session.lock().unwrap().clone().expect("session issued");
        assert!(auth.session_valid(Some(&session)));
        // 旧 token 已焚：再交换恒失败（一次性语义不变）
        assert!(!auth.exchange("first"));

        // 重建窗口：进程内重新签发
        let fresh = auth.reissue();
        assert_ne!(fresh, "first");
        assert!(auth.enabled(), "reissue 后鉴权必须仍处于启用态");
        assert!(auth.exchange(&fresh), "重新签发的 token 必须可交换");
        let session2 = auth.session.lock().unwrap().clone().expect("session reissued");
        assert!(auth.session_valid(Some(&session2)));
        // 上一轮会话随重建失效（桌面单窗口：旧 webview 已死，不构成体验退化）
        assert!(!auth.session_valid(Some(&session)));
        // 新 token 同样一次性
        assert!(!auth.exchange(&fresh));
    }

    /// 未启用鉴权（容器形态，token=None）时 reissue 会把它打开一条缝：
    /// 语义上不该被调用；此处固化「reissue 会签发 token」这一事实，避免误用成开闸。
    #[test]
    fn reissue_on_disabled_auth_turns_it_enabled() {
        let auth = Auth::new(None);
        assert!(!auth.enabled());
        let t = auth.reissue();
        assert!(auth.enabled());
        assert!(auth.exchange(&t));
    }
}

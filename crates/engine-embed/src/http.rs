//! HTTP 客户端构造（模型下载 + OpenAI 兼容请求共用）。
//!
//! 代理语义：
//! - [`agent`]：全局超时 + **env 代理**（`ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`，`NO_PROXY` 白名单生效）；
//! - [`agent_with_proxy`]：显式覆盖（`Some("")` = 强制直连，`Some("http://host:port")` = 指定代理）。
//!
//! 桌面版默认走 env 代理（公司/家庭内网用户无需配置即可下载模型）；
//! 指向 localhost 的调用方（测试、本地 vLLM/云 VL）应显式传 `Some("")` 走直连。

use std::time::Duration;

/// 构造带全局超时的 agent（秒），代理取 env。
pub fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .proxy(ureq::Proxy::try_from_env())
        .build()
        .into()
}

/// 构造带全局超时的 agent，并显式指定代理（Some("") 表示不走代理）。
pub fn agent_with_proxy(timeout_secs: u64, proxy: Option<&str>) -> ureq::Agent {
    let mut b =
        ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(timeout_secs)));
    match proxy {
        Some(p) if p.trim().is_empty() => b = b.proxy(None),
        Some(p) => {
            b = b.proxy(ureq::Proxy::new(p.trim()).ok());
        }
        None => b = b.proxy(ureq::Proxy::try_from_env()),
    }
    b.build().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_builds_with_and_without_proxy() {
        let _a = agent(5);
        let _b = agent_with_proxy(5, None);
        let _c = agent_with_proxy(5, Some("http://127.0.0.1:11700"));
        let _d = agent_with_proxy(5, Some("")); // 显式直连
    }
}

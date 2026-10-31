//! W7 桌面壳装配：内嵌 app-web（127.0.0.1 随机端口 + 一次性 token 引导）→ 开主窗口。
//!
//! 启动顺序：解析数据/模型目录 → prepare（同步：开库/迁移/首启 seeding，桌面默认
//! embedder=本地 siglip2）→ 起服务线程（axum::serve，无信号处理，随进程退出）→
//! 窗口导航到 `/auth/boot?token=..&next=/`（token 用后即焚，会话走 HttpOnly cookie）。
//!
//! 离线四态语义（R6 + M2 降级链）在 web 层原样生效：local-infer 构建（本 crate 唯一
//! 构建形态）默认 provider=local；设置页仍可切远程 dam-ai / dashscope / 自定义
//! base_url（探针失败给引导，不崩溃；不配 tag 模型时导入/搜索全流程可用）。

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// 桌面离线默认语义塔（registry/models.yaml；R6：local-infer 构建默认 provider=local）。
const DEFAULT_EMBEDDER: &str = "siglip2-base-p224-onnx";

/// D13-Lenovo 冒烟诊断：GUI 子系统下 stderr 不可见，boot 全程 + panic 写日志文件。
/// 位置 `%TEMP%/damdesk-boot.log`（每行带 PID）。
pub(crate) fn boot_log(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("damdesk-boot.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}][{}] {}", chrono_now(), std::process::id(), msg);
    }
}

fn chrono_now() -> String {
    // 无 chrono 依赖：用 SystemTime 相对毫秒即可定位卡点顺序。
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("{}ms", d.as_millis()),
        Err(_) => "?".into(),
    }
}

/// 启动内嵌 server + 主窗口（由 tauri setup 调用， panic 即启动失败显式暴露）。
pub fn boot(app: AppHandle) {
    boot_log("boot: enter");
    // 1. 数据目录：与旧桌面版同一布局（{appData}/dam.db + thumbs/ + models/），
    //    老用户升级后库资产无缝延续。
    let data_dir = app
        .path()
        .app_data_dir()
        .expect("resolve app data dir");
    std::fs::create_dir_all(&data_dir).expect("create app data dir");
    boot_log(&format!("boot: data_dir={}", data_dir.display()));

    // 2. 模型根三级解析（语义同 D13/D14a）：appData 已有任一 registry 模型 → 沿用；
    //    否则安装包内置 resources/models → 用它（离线免下载）；否则兜底 appData。
    let models_root = models_root_effective(&app, &data_dir);
    boot_log(&format!("boot: models_root={}", models_root.display()));

    // 3. 一次性 token + 装配 + 服务线程
    let token = app_web::auth::generate_token();
    // R37：鉴权句柄由壳持有 —— WebView2 崩溃重建窗口时要进程内重新签发引导 token
    let auth = std::sync::Arc::new(app_web::auth::Auth::new(Some(token.clone())));
    let resource_dir = app.path().resource_dir().ok();
    boot_log(&format!(
        "boot: resource_dir={}",
        resource_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".into())
    ));
    let cfg = app_web::ServeConfig {
        data_dir: data_dir.clone(),
        dist: Some(
            resource_dir
                .clone()
                .map(|r| r.join("web-dist"))
                .unwrap_or_else(|| data_dir.join("missing-web-dist")),
        ),
        token: Some(token.clone()),
        auth: Some(auth.clone()),
        default_embedder: Some(DEFAULT_EMBEDDER.into()),
        models_root: Some(models_root),
        // KI-28：包内资源根交给 app-web 解析（内置 python runtime + 随包启动器）。
        // 纯净 Windows 没有 `python3`，这里不给出资源根 = 行业包（合规插件）起不来。
        resource_dir,
    };
    let std_listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind embedded dam-web");
    let port = std_listener.local_addr().expect("local addr").port();
    boot_log(&format!("boot: bound 127.0.0.1:{port}"));
    // prepare 同步完成（DB/迁移/seeding 就绪）再起服务线程，窗口导航不会撞 connection refused
    let router = match app_web::prepare(&cfg) {
        Ok(r) => r,
        Err(e) => {
            boot_log(&format!("boot: prepare FAILED: {e}"));
            panic!("embedded dam-web prepare failed: {e}");
        }
    };
    boot_log("boot: prepare ok");
    // tokio::from_std 要求非阻塞 fd（且必须在 runtime 上下文内转换——见线程内）
    std_listener
        .set_nonblocking(true)
        .expect("set nonblocking");
    let thread_result = std::thread::Builder::new()
        .name("dam-web".into())
        .spawn(move || {
            boot_log("dam-web: rt building");
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime for dam-web");
            boot_log("dam-web: rt ok");
            let serve_res = rt.block_on(async move {
                // from_std 必须在 runtime 上下文内调用（否则 panic: no reactor running）
                let listener =
                    match tokio::net::TcpListener::from_std(std_listener) {
                        Ok(l) => l,
                        Err(e) => {
                            boot_log(&format!("dam-web: from_std FAILED: {e}"));
                            panic!("tokio listener: {e}");
                        }
                    };
                boot_log("dam-web: listening");
                app_web::serve_prepared(listener, router).await
            });
            match serve_res {
                Ok(()) => boot_log("dam-web: serve returned Ok"),
                Err(e) => boot_log(&format!("dam-web: serve exited Err: {e}")),
            }
        });
    match thread_result {
        Ok(_) => boot_log("boot: dam-web thread spawned"),
        Err(e) => {
            boot_log(&format!("boot: thread spawn FAILED: {e}"));
            panic!("spawn dam-web thread: {e}");
        }
    }

    // 4. 主窗口：一次性引导 URL（token 用后即焚 → 窗口刷新/重开不会被复用）
    let url = boot_url(port, &token).expect("parse boot url");
    boot_log(&format!("boot: building window url={url}"));
    match build_main_window(&app, &url) {
        Ok(_) => boot_log("boot: window built"),
        Err(e) => {
            boot_log(&format!("boot: window build FAILED: {e}"));
            panic!("build main window: {e}");
        }
    }

    // 5. R37：WebView2 进程失败自愈守卫（浏览器进程被杀 → 重建窗口；渲染进程级 → 重载）
    crate::guard::install(app.clone(), port, auth);
}

/// 主窗口 label（boot 与 WebView2 崩溃重建共用；外部探针/断言也认这个 label 与标题）。
pub(crate) const MAIN_LABEL: &str = "main";

/// 一次性引导 URL（token 用后即焚；重建窗口时必须用**新签发**的 token，见 `guard::rebuild`）。
pub(crate) fn boot_url(port: u16, token: &str) -> Result<tauri::Url, String> {
    tauri::Url::parse(&format!(
        "http://127.0.0.1:{port}/auth/boot?token={token}&next=/"
    ))
    .map_err(|e| format!("parse boot url: {e}"))
}

/// 建主窗口（boot 与 WebView2 崩溃重建**共用同一套**标题/尺寸/调试参数 ——
/// 重建后的窗口行为必须与首建一致，否则崩溃恢复会把 CDP 尾巴一起丢）。
pub(crate) fn build_main_window(
    app: &AppHandle,
    url: &tauri::Url,
) -> tauri::Result<WebviewWindow> {
    let wb = WebviewWindowBuilder::new(app, MAIN_LABEL, WebviewUrl::External(url.clone()))
        .title("DAMDesk")
        .inner_size(1280.0, 800.0)
        .min_inner_size(960.0, 600.0);
    // D13-Lenovo：WebView2 155 运行时会剥除 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS 环境变量
    // （真机实测 remote-debugging-port 不生效），远程调试走编程通道。覆盖时需带上 wry 默认
    // disable-features 集，避免回退 UI/PDF 行为变化。
    match webview_args_override(std::env::var("DAMDESK_WEBVIEW_ARGS").ok().as_deref()) {
        Some(args) => {
            boot_log(&format!("boot: additional_browser_args={args}"));
            wb.additional_browser_args(&args).build()
        }
        None => wb.build(),
    }
}

/// `DAMDESK_WEBVIEW_ARGS` 覆盖参数拼接（纯函数，可测）。D13-Lenovo：WebView2 155 运行时会
/// 剥除 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS 环境变量（真机实测 remote-debugging-port 不生效），
/// 远程调试走编程通道。覆盖时必须带上 wry 默认 disable-features 集，避免回退 UI/PDF 行为变化；
/// 未设置 / 空串 / 纯空白 → None，即不调用 `additional_browser_args`，保持 wry 默认行为。
fn webview_args_override(env_value: Option<&str>) -> Option<String> {
    let extra = env_value?.trim();
    if extra.is_empty() {
        return None;
    }
    Some(format!(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection {extra}"
    ))
}

/// 生效模型根（DT5 起逐模型回退，见 `models_bootstrap`）。返回的是 **appData 式根**，
/// 不是 `…/models` 本身 —— 权重落点由
/// `core_config::model_dir(base, name)` 拼成 `<base>/models/<name>`（engine-embed/factory 契约，
/// 见 `crates/app-web/src/infer.rs` 的说明）。**返回 `<base>/models` 会让 factory 再拼一次
/// `models` → `<base>/models/models/<name>`，权重必然找不到**（真机实测：mac 桌面版
/// `load provider: model files missing: …/Resources/models/models/siglip2-base-p224-onnx/vision.onnx`
/// → 本地嵌入起不来 → 文搜图/以图搜图/找相似全不可用）。
/// 选根口径（旧「appData 有任一模型就整根用 appData」已废）：每个内置模型先看
/// `{appData}/models/<name>`，缺了再看安装包资源目录 `models/<name>`；整根取能覆盖
/// **全部**内置模型的那个，都覆盖不了才兜底 `{appData}`（缺的模型名进 boot 日志）。
fn models_root_effective(app: &AppHandle, data_dir: &std::path::Path) -> std::path::PathBuf {
    models_bootstrap(data_dir, app.path().resource_dir().ok().as_deref())
}

// ---------- DT5：模型根逐模型回退（OCR 与 siglip 不再抢同一个根） ----------

/// 随安装包内置的模型目录名（`crates/app-tauri/resources/models/` 下的**实体**目录）。
///
/// 与 `tauri.conf.json` 的 `bundle.resources` 必须逐条一致（单测
/// `dt5_bundled_models_match_tauri_conf` 核对）：漏一条 = 包里没有该权重，而老用户
/// appData 里只有 siglip 时整根不会退回资源 → 该能力静默不可用（DT5 修的就是这个）。
/// `lama-fp32`（208MB）**不**内置（registry 注释裁决，走 model_store 分发），别加进来。
/// `dinov3-vits16-onnx` **不**内置（GH-5 合规裁决）：DINOv3 权重受 Meta DINOv3 许可约束、
/// 禁止随包再分发 → 判重塔降级为「可选塔」，缺件时 app-web 如实报
/// 「判重不可用，需自备模型（Meta 许可）」（`library_api::stats` 的 dedupAvailable），
/// 用户自备两件套放 `{models_root}/models/dinov3-vits16-onnx/` 即恢复。
const BUNDLED_MODELS: &[&str] = &[
    "siglip2-base-p224-onnx",
    "u2net",
    "realesr-general-x4v3",
    "ppocrv6-medium-det",
    "ppocrv6-medium-rec",
];

/// 模型目录是否可用：存在且非空（空目录 = 权重没解开，按「没有」处理，不猜）。
fn model_dir_usable(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut it| it.next().is_some())
}

/// 某个根下有没有该模型：`<root>/models/<name>` 可用。
fn root_has_model(root: &std::path::Path, name: &str) -> bool {
    model_dir_usable(&core_config::model_dir(root, name))
}

/// **单模型**目录逐级回退（DT5 语义）：`{appData}/models/<name>` 可用 → 用它（用户已下载/
/// 自装优先）；否则资源目录 `models/<name>` 可用 → 用资源（安装包内置，离线免下载）；
/// 都没有 → None（调用方兜底根，保持「缺模型响亮报错」，不猜路径）。
fn resolve_model_dir(
    data_dir: &std::path::Path,
    resource_dir: Option<&std::path::Path>,
    name: &str,
) -> Option<std::path::PathBuf> {
    if root_has_model(data_dir, name) {
        return Some(core_config::model_dir(data_dir, name));
    }
    let res = resource_dir?;
    root_has_model(res, name).then(|| core_config::model_dir(res, name))
}

/// 生效模型根解析结果（boot 逐条打日志 + 单测验契约）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelsPlan {
    /// 交给 app-web / 引擎的 appData 式根（权重 = `<base>/models/<name>`）
    base: std::path::PathBuf,
    /// 每个内置模型**实际会读到**的目录（= `<base>/models/<name>`）
    per_model: Vec<(String, std::path::PathBuf)>,
    /// 生效根下**缺**的内置模型名（响亮列出，不静默）
    missing: Vec<String>,
}

/// 逐模型回退后选出整根：
/// 1. 每个内置模型都能在 `{appData}/models/<name>` 用到 → appData（用户自装/已下载优先）；
/// 2. 每个都能在安装包资源目录的 `models/<name>` 用到 → 资源根（老用户 appData 只有
///    siglip 时走这条，包内 OCR/u2net/realesr 才用得上）；
/// 3. 逐模型落到不同根（一对一可以、一个根表达不了）但资源根能覆盖全部 → 资源根；
/// 4. 都不满足 → 兜底 appData（首启下载落点），`missing` 列出缺的模型名。
///
/// 为什么不能「appData 里有任意一个 registry 模型就整根用 appData」（D13/D14a 旧语义）：
/// 老用户 appData 只有 siglip2 时整根走 appData，包内 OCR 三件就永远读不到（DT5 缺陷）。
fn resolve_models(
    data_dir: &std::path::Path,
    resource_dir: Option<&std::path::Path>,
    bundled: &[&str],
) -> ModelsPlan {
    let total = bundled.len();
    let (mut from_local, mut from_bundle) = (0usize, 0usize);
    for name in bundled {
        match resolve_model_dir(data_dir, resource_dir, name) {
            Some(dir) if dir.starts_with(data_dir) => from_local += 1,
            Some(_) => from_bundle += 1,
            None => {}
        }
    }
    let bundle_covers_all = resource_dir.is_some_and(|res| {
        bundled.iter().all(|name| root_has_model(res, name))
    });
    let base = if total > 0 && from_local == total {
        data_dir.to_path_buf()
    } else if total > 0 && (from_bundle == total || bundle_covers_all) {
        // 逐模型首选分裂（如 siglip 在 appData、OCR 只在包内）也收敛到包内资源根：
        // 消费者只吃一个根，而资源根能覆盖全部内置模型。
        resource_dir.map_or_else(|| data_dir.to_path_buf(), std::path::Path::to_path_buf)
    } else {
        data_dir.to_path_buf()
    };
    let per_model: Vec<(String, std::path::PathBuf)> = bundled
        .iter()
        .map(|n| ((*n).to_string(), core_config::model_dir(&base, n)))
        .collect();
    let missing = per_model
        .iter()
        .filter(|(_, dir)| !model_dir_usable(dir))
        .map(|(name, _)| name.clone())
        .collect();
    ModelsPlan {
        base,
        per_model,
        missing,
    }
}

/// boot 的模型段：解析生效根（逐模型回退）→ 逐模型日志 → 注入引擎根。
/// 返回交给 app-web 的根（`<base>/models/<name>` 契约，**不是** `<base>/models`）。
fn models_bootstrap(
    data_dir: &std::path::Path,
    resource_dir: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let plan = resolve_models(data_dir, resource_dir, BUNDLED_MODELS);
    for (name, dir) in &plan.per_model {
        let state = if plan.missing.iter().any(|m| m == name) {
            "缺失"
        } else {
            "ok"
        };
        boot_log(&format!("boot: model {name} -> {} ({state})", dir.display()));
    }
    boot_log(&format!(
        "boot: models base={} missing={:?}",
        plan.base.display(),
        plan.missing
    ));
    let engine_root = apply_engine_models_root(
        std::env::var("DAM_MODELS_ROOT").ok().as_deref(),
        &plan.base,
    );
    boot_log(&format!(
        "boot: DAM_MODELS_ROOT={}",
        engine_root.display()
    ));
    plan.base
}

/// 引擎模型根注入：`DAM_MODELS_ROOT`（engine-ocr / engine-segment / engine-restore 契约，
/// 权重读 `<root>/models/<name>`）。已显式设置（验收脚本/容器）→ 不覆盖；
/// 未设置或纯空白 → 写生效根。返回最终生效值（单测据此验「不覆盖」语义）。
fn apply_engine_models_root(
    existing: Option<&str>,
    resolved: &std::path::Path,
) -> std::path::PathBuf {
    if let Some(v) = existing.map(str::trim).filter(|v| !v.is_empty()) {
        return std::path::PathBuf::from(v);
    }
    std::env::set_var("DAM_MODELS_ROOT", resolved);
    resolved.to_path_buf()
}

/// 旧三级解析（D13/D14a）：**仅兼容保留**，boot 已改走 `resolve_models`（生产不调用，故
/// `dead_code` 豁免）。语义收窄为「registry 里全部需要本地权重的模型都能在同一根用到才算根」。
#[allow(dead_code)]
fn resolve_models_base(
    data_dir: &std::path::Path,
    resource_dir: Option<&std::path::Path>,
    reg: Option<&core_config::Registry>,
) -> std::path::PathBuf {
    let names = local_model_names(reg);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    resolve_models(data_dir, resource_dir, &refs).base
}

/// registry 里需要本地权重的模型名（provider 以 `onnx_` 开头；远程 openai_compat 条目
/// 不需要权重，也不该参与「根是否覆盖」的判断）。只由兼容入口 `resolve_models_base` 使用。
#[allow(dead_code)]
fn local_model_names(reg: Option<&core_config::Registry>) -> Vec<String> {
    reg.map(|r| {
        r.models
            .iter()
            .filter(|m| m.provider.starts_with("onnx_"))
            .map(|m| m.name.clone())
            .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> core_config::Registry {
        core_config::Registry::from_yaml_str(
            "models:\n  - name: dinov3-vits16-onnx\n    kind: embedding\n    dims: 384\n    provider: onnx_dino\n",
        )
        .expect("parse test registry")
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let t = std::env::temp_dir().join(format!("w7-shell-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let _ = std::fs::create_dir_all(&t);
        t
    }

    /// 造一个**可用**的模型目录（非空：带一个权重文件）。
    fn seed_model(root: &std::path::Path, name: &str) {
        let dir = core_config::model_dir(root, name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("weights.onnx"), b"onnx").unwrap();
    }

    /// 造一个**不可用**的模型目录（存在但空：权没解开，按「没有」处理）。
    fn seed_empty_model(root: &std::path::Path, name: &str) {
        std::fs::create_dir_all(core_config::model_dir(root, name)).unwrap();
    }

    /// 权重落点契约：`model_dir(base, name)` 必须命中 `<base>/models/<name>`，
    /// 绝不能出现 `models/models`（mac 桌面版实测的嵌入起不来根因）。
    fn assert_weight_dir(base: &std::path::Path, name: &str) -> std::path::PathBuf {
        let dir = core_config::model_dir(base, name);
        assert!(
            !dir.to_string_lossy().contains("models/models"),
            "权重目录出现重复 models 段：{}",
            dir.display()
        );
        dir
    }

    #[test]
    fn appdata_model_wins_and_weight_dir_has_single_models_segment() {
        let tmp = tmpdir("appdata");
        let appdata = tmp.join("appdata");
        let bundled = tmp.join("bundle");
        std::fs::create_dir_all(&appdata).unwrap();
        seed_model(&appdata, "dinov3-vits16-onnx");
        seed_model(&bundled, "dinov3-vits16-onnx");
        let reg = reg();
        let base = resolve_models_base(&appdata, Some(&bundled), Some(&reg));
        assert_eq!(base, appdata, "appData 已有模型时应优先用 appData");
        assert!(assert_weight_dir(&base, "dinov3-vits16-onnx").is_dir());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bundled_models_fall_back_to_resource_root() {
        let tmp = tmpdir("bundled");
        let appdata = tmp.join("appdata");
        let bundled = tmp.join("bundle");
        std::fs::create_dir_all(appdata.join("models")).unwrap();
        seed_model(&bundled, "dinov3-vits16-onnx");
        let reg = reg();
        let base = resolve_models_base(&appdata, Some(&bundled), Some(&reg));
        assert_eq!(base, bundled, "资源目录带模型时用它当根（不是 …/bundle/models）");
        assert!(
            assert_weight_dir(&base, "dinov3-vits16-onnx").is_dir(),
            "解析出的权重目录必须真实存在"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_models_anywhere_falls_back_to_appdata() {
        let tmp = tmpdir("none");
        let appdata = tmp.join("appdata");
        let bundled = tmp.join("bundle");
        std::fs::create_dir_all(&appdata).unwrap();
        std::fs::create_dir_all(&bundled).unwrap();
        let reg = reg();
        let base = resolve_models_base(&appdata, Some(&bundled), Some(&reg));
        assert_eq!(base, appdata, "无模型资源时兜底 appData");
        assert_weight_dir(&base, "dinov3-vits16-onnx");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_registry_still_returns_appdata_style_root() {
        let tmp = tmpdir("noreg");
        let appdata = tmp.join("appdata");
        std::fs::create_dir_all(&appdata).unwrap();
        let base = resolve_models_base(&appdata, None, None);
        assert_eq!(base, appdata, "无 registry 时兜底 appData");
        assert_weight_dir(&base, "siglip2-base-p224-onnx");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---------- DT5：模型根逐模型回退（OCR 与 siglip 不再抢根） ----------

    /// 单模型三级回退：appData 可用 → 资源可用 → None；**空目录不算有**。
    #[test]
    fn dt5_resolve_model_dir_prefers_appdata_then_resource_then_none() {
        let tmp = tmpdir("dt5-per-model");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        seed_model(&appdata, "shared");
        seed_model(&res, "shared");
        seed_model(&res, "only-bundle");
        seed_empty_model(&appdata, "empty-local");
        seed_model(&res, "empty-local");

        let shared = resolve_model_dir(&appdata, Some(&res), "shared").expect("两处都有");
        assert_eq!(shared, core_config::model_dir(&appdata, "shared"));
        assert!(shared.starts_with(&appdata), "appData 可用时优先 appData");

        let bundled = resolve_model_dir(&appdata, Some(&res), "only-bundle").expect("资源有");
        assert_eq!(bundled, core_config::model_dir(&res, "only-bundle"));

        let from_res = resolve_model_dir(&appdata, Some(&res), "empty-local").expect("空目录不算有");
        assert_eq!(from_res, core_config::model_dir(&res, "empty-local"));

        assert_eq!(
            resolve_model_dir(&appdata, Some(&res), "nowhere"),
            None,
            "两处都没有 → None（不猜路径）"
        );
        assert_eq!(
            resolve_model_dir(&appdata, None, "only-bundle"),
            None,
            "没有资源目录 → None"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// DT5 核心缺陷场景：老用户 appData 只有 siglip，OCR 等内置模型只在包内 → 整根取资源根。
    #[test]
    fn dt5_old_user_with_siglip_only_falls_back_to_bundle_root() {
        let tmp = tmpdir("dt5-old-user");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        seed_model(&appdata, "siglip2-base-p224-onnx");
        for name in BUNDLED_MODELS {
            seed_model(&res, name);
        }
        let plan = resolve_models(&appdata, Some(&res), BUNDLED_MODELS);
        assert_eq!(plan.base, res, "appData 只覆盖部分内置模型 → 用包内资源根");
        assert!(plan.missing.is_empty(), "资源根覆盖全部内置模型：{:?}", plan.missing);
        assert_eq!(plan.per_model.len(), BUNDLED_MODELS.len());
        for (name, dir) in &plan.per_model {
            assert_eq!(dir, &core_config::model_dir(&res, name), "每模型按生效根取目录");
            assert!(dir.is_dir(), "{name} 的目录必须真实存在");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 全新机器（appData 空）→ 包内资源根，离线免下载。
    #[test]
    fn dt5_fresh_machine_uses_bundle_root() {
        let tmp = tmpdir("dt5-fresh");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        std::fs::create_dir_all(&appdata).unwrap();
        for name in BUNDLED_MODELS {
            seed_model(&res, name);
        }
        let plan = resolve_models(&appdata, Some(&res), BUNDLED_MODELS);
        assert_eq!(plan.base, res, "全新机器用包内资源根");
        assert!(plan.missing.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// appData 覆盖全部内置模型（用户自装/下载齐）→ appData 优先，包内资源不用。
    #[test]
    fn dt5_full_appdata_wins_over_bundle() {
        let tmp = tmpdir("dt5-full");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        for name in BUNDLED_MODELS {
            seed_model(&appdata, name);
            seed_model(&res, name);
        }
        let plan = resolve_models(&appdata, Some(&res), BUNDLED_MODELS);
        assert_eq!(plan.base, appdata, "appData 全都有 → 用 appData");
        assert!(plan.missing.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 两处都覆盖不全 → 兜底 appData，且缺的模型名逐条列出（响亮，不静默）。
    #[test]
    fn dt5_nothing_available_falls_back_to_appdata_with_missing_list() {
        let tmp = tmpdir("dt5-none");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        std::fs::create_dir_all(&appdata).unwrap();
        std::fs::create_dir_all(&res).unwrap();
        let plan = resolve_models(&appdata, Some(&res), BUNDLED_MODELS);
        assert_eq!(plan.base, appdata, "兜底 appData（首启下载落点）");
        assert_eq!(plan.missing.len(), BUNDLED_MODELS.len(), "缺的模型必须逐条报出来");
        for name in BUNDLED_MODELS {
            assert!(plan.missing.iter().any(|m| m == name), "{name} 应在 missing 里");
        }
        // 连资源目录都没有（resource_dir=None）同样兜底 appData 并报缺
        let plan_none = resolve_models(&appdata, None, &["ppocrv6-medium-det"]);
        assert_eq!(plan_none.base, appdata);
        assert_eq!(plan_none.missing, vec!["ppocrv6-medium-det".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 包内只覆盖一部分（开发机没挂全资源）→ 兜底 appData，缺的照样列出来。
    #[test]
    fn dt5_partial_bundle_falls_back_to_appdata() {
        let tmp = tmpdir("dt5-partial");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        seed_model(&appdata, "siglip2-base-p224-onnx");
        seed_model(&res, "ppocrv6-medium-det");
        let plan = resolve_models(
            &appdata,
            Some(&res),
            &["siglip2-base-p224-onnx", "ppocrv6-medium-det"],
        );
        assert_eq!(plan.base, appdata, "两边都不全 → 兜底 appData");
        assert_eq!(plan.missing, vec!["ppocrv6-medium-det".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 空内置列表（无 registry 的旧口径）→ appData，per_model 为空。
    #[test]
    fn dt5_empty_bundled_list_keeps_appdata_root() {
        let tmp = tmpdir("dt5-empty-bundled");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        std::fs::create_dir_all(&appdata).unwrap();
        std::fs::create_dir_all(&res).unwrap();
        let none: &[&str] = &[];
        let plan = resolve_models(&appdata, Some(&res), none);
        assert_eq!(plan.base, appdata);
        assert!(plan.per_model.is_empty());
        assert!(plan.missing.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// registry 里远程 provider（openai_compat）不需权重，不参与「根是否覆盖」判断。
    #[test]
    fn dt5_local_model_names_only_onnx_providers() {
        let reg = core_config::Registry::from_yaml_str(
            r#"
models:
  - name: local-a
    kind: embedding
    dims: 384
    provider: onnx_dino
  - name: remote-b
    kind: embedding
    dims: 0
    provider: openai_compat
"#,
        )
        .expect("parse registry");
        assert_eq!(local_model_names(Some(&reg)), vec!["local-a".to_string()]);
        assert!(local_model_names(None).is_empty(), "无 registry → 空列表");
    }

    /// 兼容入口 `resolve_models_base` 走同一套逐模型口径（不再「有任意一个就整根用」）。
    #[test]
    fn dt5_legacy_resolve_models_base_uses_per_model_rules() {
        let tmp = tmpdir("dt5-legacy");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        seed_model(&appdata, "dinov3-vits16-onnx");
        seed_model(&res, "dinov3-vits16-onnx");
        seed_model(&res, "u2net");
        let reg = core_config::Registry::from_yaml_str(
            r#"
models:
  - name: dinov3-vits16-onnx
    kind: embedding
    dims: 384
    provider: onnx_dino
  - name: u2net
    kind: segmentation
    dims: 0
    provider: onnx_u2net
"#,
        )
        .expect("parse registry");
        assert_eq!(
            resolve_models_base(&appdata, Some(&res), Some(&reg)),
            res,
            "appData 只有 dinov3、包内还有 u2net → 整根取包内（旧语义会错选 appData）"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 还原 `DAM_MODELS_ROOT`（env 是进程级全局态，用例必须自己收拾干净）。
    fn restore_models_root_env(saved: Option<String>) {
        match saved {
            Some(v) => std::env::set_var("DAM_MODELS_ROOT", v),
            None => std::env::remove_var("DAM_MODELS_ROOT"),
        }
    }

    /// 还原函数两个分支都走一遍（有旧值 → 写回；无旧值 → 删除）。
    #[test]
    fn dt5_restore_models_root_env_covers_both_branches() {
        let _g = core_config::models_root_lock();
        restore_models_root_env(Some("/models".to_string()));
        assert_eq!(std::env::var("DAM_MODELS_ROOT").unwrap(), "/models");
        restore_models_root_env(None);
        assert!(std::env::var("DAM_MODELS_ROOT").is_err(), "无旧值 → 删掉");
    }

    /// 引擎根注入：显式设置（验收脚本/容器）不覆盖；纯空白/未设置才写生效根。
    #[test]
    fn dt5_engine_root_env_respects_explicit_and_blank() {
        let _g = core_config::models_root_lock();
        let saved = std::env::var("DAM_MODELS_ROOT").ok();
        let resolved = std::path::Path::new("/tmp/dt5-resolved");

        std::env::set_var("DAM_MODELS_ROOT", "/models");
        assert_eq!(
            apply_engine_models_root(Some("  /models "), resolved),
            std::path::PathBuf::from("/models"),
            "显式设置 → 不覆盖（trim 后原样用）"
        );
        assert_eq!(std::env::var("DAM_MODELS_ROOT").unwrap(), "/models");

        std::env::set_var("DAM_MODELS_ROOT", "   ");
        assert_eq!(
            apply_engine_models_root(Some("   "), resolved),
            resolved,
            "纯空白 → 写生效根"
        );
        assert_eq!(
            std::env::var("DAM_MODELS_ROOT").unwrap(),
            "/tmp/dt5-resolved"
        );

        std::env::remove_var("DAM_MODELS_ROOT");
        assert_eq!(
            apply_engine_models_root(None, resolved),
            resolved,
            "未设置 → 写生效根"
        );
        assert_eq!(
            std::env::var("DAM_MODELS_ROOT").unwrap(),
            "/tmp/dt5-resolved"
        );

        restore_models_root_env(saved);
    }

    /// boot 模型段：逐模型落日志（含缺失分支）+ 注入引擎根 + 返回 appData 式根。
    #[test]
    fn dt5_models_bootstrap_logs_models_and_injects_engine_root() {
        let _g = core_config::models_root_lock();
        let saved = std::env::var("DAM_MODELS_ROOT").ok();
        std::env::remove_var("DAM_MODELS_ROOT");
        let tmp = tmpdir("dt5-bootstrap");
        let appdata = tmp.join("appdata");
        let res = tmp.join("bundle");
        for name in BUNDLED_MODELS {
            seed_model(&res, name);
        }
        let base = models_bootstrap(&appdata, Some(&res));
        assert_eq!(base, res);
        assert!(
            !base.to_string_lossy().ends_with("/models"),
            "返回 appData 式根，不是 …/models"
        );
        assert_eq!(
            std::env::var("DAM_MODELS_ROOT").unwrap(),
            res.to_string_lossy().to_string(),
            "引擎根必须注入生效根（engine-ocr 靠它找权重）"
        );

        // 缺失分支：资源也没有 → 兜底 appData，日志里逐条标「缺失」
        let empty_res = tmp.join("empty");
        std::fs::create_dir_all(&empty_res).unwrap();
        let base2 = models_bootstrap(&appdata, Some(&empty_res));
        assert_eq!(base2, appdata, "两处都没有 → 兜底 appData");
        assert_eq!(
            std::env::var("DAM_MODELS_ROOT").unwrap(),
            res.to_string_lossy().to_string(),
            "已注入的引擎根不被二次覆盖（显式设置优先，同进程只注入一次）"
        );

        restore_models_root_env(saved);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `BUNDLED_MODELS` 与 `tauri.conf.json` 的 `bundle.resources` 必须一一对应：
    /// 漏一条 = 包里没权重（选根再聪明也救不回）；多一条 = 打包期 glob 失败。
    #[test]
    fn dt5_bundled_models_match_tauri_conf() {
        let conf = include_str!("../tauri.conf.json");
        for name in BUNDLED_MODELS {
            let key = format!("\"resources/models/{name}/**/*\": \"models/{name}/\"");
            assert!(conf.contains(&key), "tauri.conf.json 缺 resources 条目：{key}");
        }
        assert_eq!(
            conf.matches("\"resources/models/").count(),
            BUNDLED_MODELS.len(),
            "模型资源条目数必须与 BUNDLED_MODELS 一致"
        );
    }

    /// `DAMDESK_WEBVIEW_ARGS` 拼接契约：默认 disable-features 前缀 + 空格 + extra。
    const WRY_DEFAULT_DISABLE_FEATURES: &str =
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

    #[test]
    fn webview_args_override_prefixes_wry_default_disable_features() {
        let args = webview_args_override(Some("--remote-debugging-port=9223")).expect("有值必覆盖");
        assert_eq!(
            args,
            format!("{WRY_DEFAULT_DISABLE_FEATURES} --remote-debugging-port=9223"),
            "必须是 wry 默认 disable-features 前缀 + 空格 + extra"
        );
    }

    #[test]
    fn webview_args_override_trims_extra_and_joins_with_single_space() {
        let args = webview_args_override(Some(
            "  --remote-debugging-port=9223 --remote-allow-origins=* \n",
        ))
        .expect("有值必覆盖");
        assert_eq!(
            args,
            format!(
                "{WRY_DEFAULT_DISABLE_FEATURES} --remote-debugging-port=9223 --remote-allow-origins=*"
            ),
            "extra 首尾空白剥掉，与前缀之间恰好一个空格"
        );
    }

    #[test]
    fn webview_args_override_blank_or_unset_means_no_override() {
        assert_eq!(webview_args_override(None), None, "env 未设置 → 不覆盖");
        assert_eq!(webview_args_override(Some("")), None, "空串 → 不覆盖");
        assert_eq!(
            webview_args_override(Some("   \t\n")),
            None,
            "纯空白 → 不覆盖（保持 wry 默认）"
        );
    }
}

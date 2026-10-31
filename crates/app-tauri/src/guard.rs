//! R37：WebView2 进程失败自愈（BATCH-04 / B04-3 真机缺陷的修复腿）。
//!
//! 缺陷现场：应用运行中外部杀掉 WebView2 的进程（`Stop-Process msedgewebview2`）后，
//! `app-tauri.exe` 本体存活但窗口实质作废——白屏、CDP 端点消失、无任何可读提示，用户只能
//! 手动重启应用。根因：wry/Tauri 不订阅 WebView2 的 `ProcessFailed`，渲染环境死了宿主不知道。
//!
//! 修复（两条腿，按失败类型分流，见 `decide`）：
//! - `BROWSER_PROCESS_EXITED`(0)：整个渲染环境作废，只有重建 webview 能恢复 → 销毁旧窗口，
//!   用**新签发的一次性 token** 重建同 label 窗口（token 走 `app_web::auth::Auth::reissue`，
//!   原 token 在启动交换时已焚毁），重建后再挂钩监听。
//! - 其余 kind（渲染/GPU/工具进程退出、渲染进程无响应）：webview 本体还在，`reload()` 页面即可。
//! - 60s 窗口内已重建 ≥ `MAX_REBUILDS` 次：判定持续性崩溃，停止重建，落日志 + 弹系统对话框给
//!   可读错误（B04-3 判据「给出可读错误 **或** 自动重载」的另一条腿）。
//!
//! 证据通道：每一步都写 `boot_log`（`%TEMP%/damdesk-boot.log`），真机探针据此断言
//! 「订阅成功 / 收到 kind=0 / 窗口已重建」，不靠猜测。
//!
//! 非 Windows 构建没有 webview 可订阅（`attach` 是空实现），判定逻辑只剩单测在调它 ——
//! 这里放行 dead_code，避免 Linux/CI 被纯判定函数的「未被引用」警告淹没。

#![cfg_attr(not(windows), allow(dead_code))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::AppHandle;
use tauri::Manager;

use crate::shell::{boot_log, boot_url, build_main_window, MAIN_LABEL};

/// 60s 窗口内允许的最大重建次数（防「崩溃 → 重建 → 再崩」死循环）。
pub(crate) const MAX_REBUILDS: usize = 3;

/// 重建计数的时间窗口：超过它就认为上一轮崩溃已经过去，预算重新计。
pub(crate) const REBUILD_WINDOW: Duration = Duration::from_secs(60);

/// WebView2 `COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED` 的取值
/// （webview2-com-sys 0.39.1 bindings.rs:703；这里用字面量 + 单测，避免为纯判定引依赖）。
pub(crate) const KIND_BROWSER_PROCESS_EXITED: i32 = 0;

/// 收到 `ProcessFailed` 后的恢复动作（纯判定结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// 渲染进程级失败：页面重载即可（webview 本体可用，不闪窗口、不丢窗口状态）
    Reload,
    /// 浏览器进程退出：渲染环境整块作废 → 重建 webview（= 重建窗口）
    Rebuild,
    /// 重建次数超预算：放弃自动恢复，给可读错误
    GiveUp,
}

/// 失败类型 + 近期已用重建次数 → 恢复动作（**纯函数**，Linux 上可测）。
pub(crate) fn decide(kind: i32, recent_rebuilds: usize) -> Action {
    if kind != KIND_BROWSER_PROCESS_EXITED {
        // 渲染/GPU/工具进程退出、渲染进程无响应：Chromium 自己会重拉子进程，
        // 重载一次页面即可把白页/卡死换掉；重建窗口反而会闪屏并丢 UI 状态。
        return Action::Reload;
    }
    if recent_rebuilds >= MAX_REBUILDS {
        return Action::GiveUp;
    }
    Action::Rebuild
}

/// 计数窗口折算：`count` 次重建距今 `elapsed` —— 超过窗口即归零（纯函数）。
pub(crate) fn count_in_window(count: usize, elapsed: Duration) -> usize {
    if elapsed > REBUILD_WINDOW {
        0
    } else {
        count
    }
}

/// 两次恢复动作之间的最小间隔（同一轮崩溃会连发好几条 ProcessFailed）。
pub(crate) const ACTION_GAP: Duration = Duration::from_millis(800);

/// 建替代窗口的最大尝试次数（只对「旧窗口 label 还没被销毁完」这种瞬时冲突重试）。
pub(crate) const MAX_REPLACE_ATTEMPTS: u32 = 3;

/// 替代窗口重试间隔：给事件循环把 destroy 消息处理掉的时间。
pub(crate) const REPLACE_RETRY_GAP: Duration = Duration::from_millis(400);

/// 距上次动作不足 `ACTION_GAP` → 本轮事件跳过（纯函数）。
pub(crate) fn throttled(last: Option<Duration>) -> bool {
    matches!(last, Some(d) if d < ACTION_GAP)
}

/// 崩溃自愈守卫：持有重建所需的全部上下文（app / 端口 / 鉴权句柄 / 重建预算）。
pub(crate) struct Guard {
    app: AppHandle,
    port: u16,
    auth: Arc<app_web::auth::Auth>,
    /// (窗口内已用次数, 窗口起点)
    budget: Mutex<(usize, Instant)>,
    /// 上一次恢复动作时间（节流用）
    last_action: Mutex<Option<Instant>>,
    /// 重建进行中：期间到来的事件一律丢弃（避免重入）
    rebuilding: AtomicBool,
}

/// 「零窗口」瞬间：重建时先销毁旧窗口再建新窗口，这段时间不能让 Tauri 顺势退出。
static RECOVERING: AtomicBool = AtomicBool::new(false);

/// 进程内唯一守卫（桌面壳只有一个窗口，无需 state 注入的复杂度）。
static GUARD: OnceLock<Arc<Guard>> = OnceLock::new();

/// 正在重建窗口（`RunEvent::ExitRequested` 守卫据此决定是否放行退出）。
pub(crate) fn is_recovering() -> bool {
    RECOVERING.load(Ordering::SeqCst)
}

/// 窗口建好后安装守卫（boot 与重建腿都调用）；重复调用只重新挂钩监听。
pub(crate) fn install(app: AppHandle, port: u16, auth: Arc<app_web::auth::Auth>) {
    let guard = GUARD
        .get_or_init(|| {
            Arc::new(Guard {
                app: app.clone(),
                port,
                auth,
                budget: Mutex::new((0, Instant::now())),
                last_action: Mutex::new(None),
                rebuilding: AtomicBool::new(false),
            })
        })
        .clone();
    attach(&guard);
}

impl Guard {
    /// 本次重建前已用次数（不占额度）。
    fn used_rebuilds(&self) -> usize {
        let mut b = self.budget.lock().unwrap();
        b.0 = count_in_window(b.0, b.1.elapsed());
        if b.0 == 0 {
            b.1 = Instant::now();
        }
        b.0
    }

    /// 占用一次重建额度。
    fn take_rebuild(&self) {
        let mut b = self.budget.lock().unwrap();
        b.0 = count_in_window(b.0, b.1.elapsed());
        if b.0 == 0 {
            b.1 = Instant::now();
        }
        b.0 += 1;
    }

    /// 节流判定：距上次动作过近 → true（并记下本次动作时间）。
    fn throttle(&self) -> bool {
        let mut last = self.last_action.lock().unwrap();
        let gap = last.map(|t| t.elapsed());
        if throttled(gap) {
            return true;
        }
        *last = Some(Instant::now());
        false
    }

    /// WebView2 线程上收到的失败事件（**不在此线程做重活**，见 `post_to_main`）。
    fn on_process_failed(self: &Arc<Self>, kind: i32) {
        if self.rebuilding.load(Ordering::SeqCst) {
            boot_log(&format!("guard: ProcessFailed kind={kind} ignored (rebuild in progress)"));
            return;
        }
        let used = self.used_rebuilds();
        let action = decide(kind, used);
        boot_log(&format!(
            "guard: ProcessFailed kind={kind} used_rebuilds={used} -> {action:?}"
        ));
        if self.throttle() {
            boot_log("guard: throttled (same crash burst)");
            return;
        }
        match action {
            Action::Reload => self.post_to_main(|g| g.reload()),
            Action::Rebuild => self.post_to_main(|g| g.rebuild()),
            Action::GiveUp => self.post_to_main(|g| g.give_up()),
        }
    }

    /// 把恢复动作挪到 Tauri 主线程执行。
    ///
    /// 两个硬约束：① 建/销毁窗口只能在主线程；② `ProcessFailed` 回调跑在 WebView2 自己的
    /// 消息泵线程上，在里面同步做窗口操作会和 WebView2 的消息泵互锁 → 先甩到独立线程，再由
    /// 事件循环调度。
    fn post_to_main<F>(self: &Arc<Self>, f: F)
    where
        F: FnOnce(&Arc<Guard>) + Send + 'static,
    {
        let guard = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let app = guard.app.clone();
            let g = guard.clone();
            if let Err(e) = app.run_on_main_thread(move || f(&g)) {
                boot_log(&format!("guard: run_on_main_thread FAILED: {e}"));
            }
        });
    }

    /// 渲染进程级失败：重载当前页。
    fn reload(&self) {
        match self.app.get_webview_window(MAIN_LABEL) {
            Some(w) => match w.reload() {
                Ok(()) => boot_log("guard: reload ok"),
                Err(e) => boot_log(&format!("guard: reload FAILED: {e}")),
            },
            None => boot_log("guard: reload skipped (no main window)"),
        }
    }

    /// 浏览器进程退出：销毁旧窗口 → 新 token → 重建同 label 窗口 → 重新挂钩。
    ///
    /// 真机踩到的坑（R37 首跑）：`Window::destroy()` 只是把销毁消息**投进事件循环**，
    /// 同一次主线程回调里立刻建同名窗口必然撞 `a webview with label 'main' already exists`
    /// → 修复腿变成「每次都放弃」。所以销毁与重建必须分成两次主线程任务（FIFO：销毁先执行）。
    fn rebuild(self: &Arc<Self>) {
        if self.rebuilding.swap(true, Ordering::SeqCst) {
            boot_log("guard: rebuild already in progress, event ignored");
            return;
        }
        RECOVERING.store(true, Ordering::SeqCst);
        self.take_rebuild();
        boot_log(&format!(
            "guard: rebuild begin (used={} of max={MAX_REBUILDS})",
            self.used_rebuilds()
        ));

        if let Some(old) = self.app.get_webview_window(MAIN_LABEL) {
            match old.destroy() {
                Ok(()) => boot_log("guard: old window destroyed"),
                Err(e) => boot_log(&format!("guard: old window destroy failed: {e}")),
            }
        }
        self.post_replacement(1);
    }

    /// 把「建替代窗口」排到下一次主线程任务（排在 destroy 消息之后）。
    fn post_replacement(self: &Arc<Self>, attempt: u32) {
        let app = self.app.clone();
        let g = self.clone();
        if let Err(e) = app.run_on_main_thread(move || g.spawn_replacement(attempt)) {
            boot_log(&format!("guard: schedule replacement FAILED: {e}"));
            self.finish_rebuild(false);
            self.give_up();
        }
    }

    /// 真正建替代窗口。label 仍被占（销毁还没落地）时有限重试，不无限循环。
    fn spawn_replacement(self: &Arc<Self>, attempt: u32) {
        // 旧引导 token 在启动交换时已焚毁：重建窗口必须走新签发的一次性 token
        let token = self.auth.reissue();
        let url = match boot_url(self.port, &token) {
            Ok(u) => u,
            Err(e) => {
                boot_log(&format!("guard: boot url FAILED: {e}"));
                self.finish_rebuild(false);
                self.give_up();
                return;
            }
        };

        match build_main_window(&self.app, &url) {
            Ok(w) => {
                let _ = w.show();
                let _ = w.set_focus();
                boot_log(&format!("guard: window rebuilt url={url}"));
                // 新 webview 重新挂钩：不然第二次崩溃就没人管了
                attach(self);
                self.finish_rebuild(true);
            }
            Err(e) => {
                let label_busy = e.to_string().contains("already exists");
                boot_log(&format!(
                    "guard: rebuild attempt {attempt} FAILED (label_busy={label_busy}): {e}"
                ));
                if label_busy && attempt < MAX_REPLACE_ATTEMPTS {
                    let app = self.app.clone();
                    let g = self.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(REPLACE_RETRY_GAP);
                        let _ = app.run_on_main_thread(move || g.spawn_replacement(attempt + 1));
                    });
                    return;
                }
                self.finish_rebuild(false);
                self.give_up();
            }
        }
    }

    fn finish_rebuild(&self, ok: bool) {
        if !ok {
            boot_log("guard: rebuild aborted");
        }
        self.rebuilding.store(false, Ordering::SeqCst);
        RECOVERING.store(false, Ordering::SeqCst);
    }

    /// 自动恢复彻底失败：给一条**人能读懂**的错误（弹系统对话框）。
    fn give_up(&self) {
        boot_log("guard: give up -> readable error dialog");
        use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
        self.app
            .dialog()
            .message(format!(
                "界面进程（WebView2）反复崩溃，已尝试自动恢复 {MAX_REBUILDS} 次仍未成功。\n\n\
                 请关闭应用后重新启动（图库数据已保存，不会丢失）。\n\n\
                 诊断日志：{}",
                std::env::temp_dir().join("damdesk-boot.log").display()
            ))
            .title("DAMDesk 界面恢复失败")
            .kind(MessageDialogKind::Error)
            .blocking_show();
    }
}

// ---------- 平台腿 ----------

/// Windows：订阅 WebView2 的 `ProcessFailed`。
///
/// wry/Tauri 不透出这个事件，只能用 `PlatformWebview::controller()` 拿到 `ICoreWebView2`，
/// 再 cast 到 `ICoreWebView2_2`（`add_ProcessFailed` 在 2 号接口上）。
#[cfg(windows)]
fn attach(guard: &Arc<Guard>) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2ProcessFailedEventArgs, ICoreWebView2_2, COREWEBVIEW2_PROCESS_FAILED_KIND,
    };
    use webview2_com::ProcessFailedEventHandler;
    use windows_core::Interface;

    let Some(win) = guard.app.get_webview_window(MAIN_LABEL) else {
        boot_log("guard: attach skipped (no main window)");
        return;
    };
    let g = guard.clone();
    let dispatched = win.with_webview(move |webview| {
        let controller = webview.controller();
        // SAFETY：controller 来自刚建好的 WebView2；两个 COM 调用都在 WebView2 线程上，
        // 且 CoreWebView2 由 controller 持有（进程内句柄），不会跨线程误用。
        let core = match unsafe { controller.CoreWebView2() } {
            Ok(c) => c,
            Err(e) => {
                boot_log(&format!("guard: CoreWebView2 FAILED: {e}"));
                return;
            }
        };
        let core2: ICoreWebView2_2 = match core.cast() {
            Ok(c) => c,
            Err(e) => {
                boot_log(&format!("guard: cast ICoreWebView2_2 FAILED: {e}"));
                return;
            }
        };
        let handler = ProcessFailedEventHandler::create(Box::new(
            move |_sender: Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2>,
                  args: Option<ICoreWebView2ProcessFailedEventArgs>| {
                let kind = match &args {
                    Some(a) => {
                        // ProcessFailedKind 是 out-param 形式（webview2-com 生成绑定）
                        let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND(0);
                        match unsafe { a.ProcessFailedKind(&mut kind) } {
                            Ok(()) => kind.0,
                            Err(_) => -1,
                        }
                    }
                    None => -1,
                };
                g.on_process_failed(kind);
                Ok(())
            },
        ));
        let mut token = 0i64;
        // handler 的活命由 COM 引用计数保证（add_ProcessFailed 会 AddRef），这里 drop 不影响。
        match unsafe { core2.add_ProcessFailed(&handler, &mut token) } {
            Ok(()) => boot_log(&format!("guard: ProcessFailed subscribed (token={token})")),
            Err(e) => boot_log(&format!("guard: add_ProcessFailed FAILED: {e}")),
        }
    });
    if let Err(e) = dispatched {
        boot_log(&format!("guard: with_webview dispatch FAILED: {e}"));
    }
}

/// 非 Windows：桌面壳只跑 Windows（WebView2）；其他平台无此失败面，守卫保持惰性
/// （`decide`/预算逻辑仍是纯函数，Linux 上照样可测）。
#[cfg(not(windows))]
fn attach(_guard: &Arc<Guard>) {
    boot_log("guard: non-windows platform, no WebView2 process to watch");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 浏览者进程退出 = 渲染环境整块作废 → 必须重建（B04-3 现场就是这个 kind）。
    #[test]
    fn browser_process_exit_rebuilds() {
        assert_eq!(decide(KIND_BROWSER_PROCESS_EXITED, 0), Action::Rebuild);
        assert_eq!(decide(KIND_BROWSER_PROCESS_EXITED, MAX_REBUILDS - 1), Action::Rebuild);
    }

    /// 渲染进程级失败不动窗口（重载即可），且**永不**因预算耗尽弹错误。
    #[test]
    fn renderer_level_failures_only_reload() {
        for kind in [1, 2, 3, 4, 5, 6, 7, 8, 9, -1] {
            assert_eq!(decide(kind, 0), Action::Reload, "kind={kind}");
            assert_eq!(decide(kind, MAX_REBUILDS), Action::Reload, "kind={kind}");
            assert_eq!(decide(kind, 99), Action::Reload, "kind={kind}");
        }
    }

    /// 持续性崩溃（预算耗尽）→ 停止重建、改给可读错误（不无限重启）。
    #[test]
    fn budget_exhausted_gives_up() {
        assert_eq!(decide(KIND_BROWSER_PROCESS_EXITED, MAX_REBUILDS), Action::GiveUp);
        assert_eq!(decide(KIND_BROWSER_PROCESS_EXITED, MAX_REBUILDS + 5), Action::GiveUp);
    }

    /// 计数窗口：窗口内保留计数，超过窗口归零（下一轮崩溃重新给满预算）。
    #[test]
    fn rebuild_budget_window_rolls_over() {
        assert_eq!(count_in_window(3, Duration::from_secs(1)), 3);
        assert_eq!(
            count_in_window(3, REBUILD_WINDOW + Duration::from_secs(1)),
            0
        );
    }

    /// 同一轮崩溃连发多条 ProcessFailed 时只处理第一条。
    #[test]
    fn repeated_events_in_burst_are_throttled() {
        assert!(!throttled(None), "首条事件必须放行");
        assert!(throttled(Some(Duration::from_millis(10))));
        assert!(!throttled(Some(ACTION_GAP)));
        assert!(!throttled(Some(Duration::from_secs(5))));
    }
}

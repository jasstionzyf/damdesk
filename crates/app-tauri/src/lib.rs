//! L3 `app-tauri` — W7 桌面壳（R3 终态）。
//!
//! Tauri 窗口内嵌 **同一个 app-web**（localhost 随机端口 + 一次性 token 引导，见 shell），
//! 前端 web/desktop 一套 bundle；旧 invoke IPC 命令层已整体下线（UI 双份维护的根源）。
//! 桌面构建 feature `local-infer`（经 app-web 转发 engine-embed）：siglip2/dinov3 ONNX
//! 编进同一进程，离线默认 provider=local（R6）；模型根三级解析语义不变（D13/D14a）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod guard;
mod shell;

pub fn run() {
    // D13-Lenovo 冒烟诊断：GUI 子系统 stderr 不可见，panic 全部落 %TEMP%/soujpg-dam-boot.log
    std::panic::set_hook(Box::new(|info| {
        shell::boot_log(&format!("PANIC: {info}"));
    }));
    let app = tauri::Builder::default()
        // W7+：唯一的原生能力 —— 系统文件夹选择框（web 里拿不到本地目录绝对路径）。
        // 前端 web/src/lib/native.ts 运行时探测 __TAURI_INTERNALS__，容器形态不显示入口。
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            shell::boot(app.handle().clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(|_app_handle, event| {
        // R37：WebView2 崩溃自愈要「先销毁旧窗口、再建新窗口」，中间有一瞬零窗口。
        // 不让这一瞬被当成「用户关掉最后一个窗口」→ 否则自愈还没建完进程就退了。
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if guard::is_recovering() {
                api.prevent_exit();
            }
        }
    });
}

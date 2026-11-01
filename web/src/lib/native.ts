/**
 * 桌面壳原生能力桥：web 拿不到的能力走 Tauri，容器/浏览器形态自动回退。
 *
 * 双形态共享同一份 web/dist —— 桌面壳内嵌 app-web（有 `__TAURI_INTERNALS__`，可调
 * 系统文件夹选择框），容器 `damdesk` 只有 HTTP（无 Tauri 运行时）。所以这里只做
 * 「运行时探测 + 能力缺失返回 null」，调用方负责回退（按钮不显示 / 手填路径），
 * **绝不无条件 invoke**，否则网页形态直接崩。
 *
 * 权限最小化：仅 `dialog:allow-open`（见 crates/app-tauri/capabilities/default.json），
 * 不做任何文件系统读写——选中的路径只是回填给 `/api/import/scan` 由服务端扫描。
 */

import { tGlobal } from '../i18n'

/** Tauri v2 暴露给 webview 的 IPC 通道（只用到 invoke 一个成员）。 */
interface TauriInternals {
  invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>
}

function internals(): TauriInternals | null {
  if (typeof window === 'undefined') return null
  const w = window as unknown as { __TAURI_INTERNALS__?: TauriInternals }
  return typeof w.__TAURI_INTERNALS__?.invoke === 'function' ? w.__TAURI_INTERNALS__ : null
}

/** 当前是否运行在 Tauri 桌面壳内（容器 / 浏览器形态返回 false）。 */
export function isDesktopShell(): boolean {
  return internals() !== null
}

/**
 * 打开系统文件夹选择框，返回所选目录的绝对路径；非桌面形态（或用户取消）返回 null。
 *
 * 命令名与官方 `@tauri-apps/plugin-dialog` 的 `open()` 一致（`plugin:dialog|open`
 * + `{ options }`），这里手写以免给双形态共享的 web bundle 引入 Tauri 运行时依赖。
 */
export async function pickFolder(title = tGlobal('选择图片目录')): Promise<string | null> {
  const t = internals()
  if (!t) return null
  const picked = await t.invoke('plugin:dialog|open', {
    options: { directory: true, multiple: false, title },
  })
  return typeof picked === 'string' && picked.length > 0 ? picked : null
}

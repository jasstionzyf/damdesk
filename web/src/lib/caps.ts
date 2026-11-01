/**
 * WEBCAP：运行形态能力开关（capability flags）——前端**唯一**读口，零形态分支。
 *
 * 来源：`GET /api/capabilities`（服务端算，见 crates/svc-library/src/capabilities.rs）。
 * 前端不许自己判形态（`__TAURI_INTERNALS__` / location / UA 都不算数）：
 * 容器形态的「本地磁盘」是服务器文件系统，桌面形态才是用户自己的机器 —— 这件事只有
 * 服务端知道（挂载可写性 / 设备号是实测的），所以 flag 由服务端给、UI 只照着灰置。
 *
 * 用法：
 *   const caps = useCapabilities()            // null = 还没加载（此时不灰置，避免闪烁）
 *   can(caps, 'moveAsset')                    // 形态级开关
 *   canMoveTo(caps, dir)                      // 形态 + 该目录是否可写库根（逐目录灰置）
 *   reasonFor(caps, 'moveAsset')              // 人话原因（tooltip / 可见提示）
 *
 * 未加载（caps === null）时 `can()` 恒 true：宁可短暂显示可用，也不要在首帧把入口
 * 灰掉再跳回来（用户看不到「为什么刚才点不动」）。
 */
import { useEffect, useState } from 'react'
import { api, type Capabilities } from './api'

export type FeatureKey =
  | 'moveAsset'
  | 'renameAsset'
  | 'relink'
  | 'openInOS'
  | 'physicalDelete'
  | 'copyInImport'
  | 'scanExternalPath'

let cached: Capabilities | null = null
let inflight: Promise<Capabilities> | null = null
const listeners = new Set<() => void>()

function publish() {
  for (const l of listeners) l()
}

/** 拉一次能力开关（模块级缓存：整个会话只请求一次）。 */
export function loadCapabilities(): Promise<Capabilities> {
  if (cached) return Promise.resolve(cached)
  if (!inflight) {
    inflight = api
      .capabilities()
      .then((c) => {
        cached = c
        publish()
        return c
      })
      .catch((e) => {
        inflight = null // 允许下次重试；失败不灰置任何入口（后端老版本也照常能点）
        throw e
      })
  }
  return inflight
}

/** 同步快照（不触发请求）：给事件回调 / 测试用。 */
export function peekCapabilities(): Capabilities | null {
  return cached
}

/** 测试辅助：注入一份能力开关（单测不启后端）。 */
export function __setCapabilitiesForTest(c: Capabilities | null) {
  cached = c
  inflight = null
  publish()
}

/** React 读口：挂载时若无缓存则拉一次；返回 null 表示尚未加载。 */
export function useCapabilities(): Capabilities | null {
  const [caps, setCaps] = useState<Capabilities | null>(cached)
  useEffect(() => {
    let alive = true
    const sync = () => {
      if (alive) setCaps(cached)
    }
    listeners.add(sync)
    if (!cached) void loadCapabilities().catch(() => {})
    sync()
    return () => {
      alive = false
      listeners.delete(sync)
    }
  }, [])
  return caps
}

/** 能力是否可用（未加载 → 按可用处理，见文件头）。 */
export function can(caps: Capabilities | null, key: FeatureKey): boolean {
  if (!caps) return true
  return caps.features?.[key] !== false
}

/** 被禁用能力的人话原因（可用时为空串）。 */
export function reasonFor(caps: Capabilities | null, key: FeatureKey): string {
  return caps?.featureReasons?.[key] ?? ''
}

/**
 * 路径归一化（**只用于比较**，别拿返回值当路径用）：
 * - `\` 归成 `/`：Windows 真机给的是 `C:\Users\…`，服务端 `writableRoots` 也是反斜杠，
 *   不归一的话 `startsWith(root + '/')` 永远不命中 → 子目录全被灰置。
 * - 盘符统一大写：Windows 不区分盘符大小写（`c:\x` 与 `C:\x` 是同一个目录）。
 * - 去掉尾部 `/`：根形态 POSIX 是 `/`，Windows 盘根是 `C:`（`C:` 本身就是根，不要补 `/`，
 *   否则 `C:/` + `/` 拼出 `C://` 命中不了 `C:/Users`）。
 */
function norm(p: string): string {
  let t = p.replace(/\\/g, '/').replace(/\/+$/, '')
  if (t === '') return '/'
  if (/^[A-Za-z]:/.test(t)) t = t[0].toUpperCase() + t.slice(1)
  return t
}

/** 目录是否属于可写库根（`/data` 不含 `/database` —— 按路径段比）。 */
export function isWritableTarget(caps: Capabilities | null, dir: string): boolean {
  if (!caps) return true
  const d = norm(dir)
  return (caps.writableRoots ?? []).some((r) => {
    const root = norm(r)
    return root === '/' || d === root || d.startsWith(root + '/')
  })
}

/** 「移动到目录」对该目录是否可用 = 形态支持 + 目录在可写库根内。 */
export function canMoveTo(caps: Capabilities | null, dir: string): boolean {
  return can(caps, 'moveAsset') && isWritableTarget(caps, dir)
}

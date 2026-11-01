/**
 * PROC-RES-3（KI-40，卡 t_823483c7）：处理产物「下载」的唯一交互口径。
 *
 * 表格单元格与预览浮层共用这一份，不许各写一套：
 * 1. **同产物 1.5s 内只触发一次** —— 双击/手抖不产生两个浏览器下载任务（`claimDownload`）；
 * 2. **点完就给反馈** —— toast 说「开始下载」（不说「已下载」：原生 `<a download>` 的完成事件前端拿不到，
 *    谎报「已下载」正是 KI-40 那类假成功的同类问题）；
 * 3. **失败要说话** —— `HEAD` 探活：404（产物已不在磁盘）/400（路径不允许读）/5xx → error toast（`probeArtifact`）；
 *    405（服务端不出 HEAD）与网络层异常不判定，宁可不说话也不谎报失败。
 *
 * 下载本体仍走原生 `<a href={artifactUrl(p, true)}>`（同源 + `Content-Disposition: attachment`）：
 * 容器形态与桌面 WebView2 同一个 URL，不引入 Tauri 插件、不落临时文件。
 */
import { useCallback } from 'react'
import type { MouseEvent as ReactMouseEvent } from 'react'

import { artifactUrl } from './api'
import { useToast } from './toast'
import { tGlobal } from '../i18n'

/** 同产物重复点击去重窗口（毫秒） */
export const DOWNLOAD_DEBOUNCE_MS = 1500

/** 去重表：path → 上次点击时刻。进程内共享（表格与预览浮层点的是同一个文件） */
const lastAt = new Map<string, number>()

/** 占位一次下载机会；`false` = 窗口期内重复点击（调用方应 `preventDefault`）。 */
export function claimDownload(path: string, now: number = Date.now()): boolean {
  const prev = lastAt.get(path) ?? 0
  if (now - prev < DOWNLOAD_DEBOUNCE_MS) return false
  lastAt.set(path, now)
  // 条目不会无限增长：每次只留最近 500 个产物
  if (lastAt.size > 500) {
    for (const k of [...lastAt.keys()].slice(0, lastAt.size - 500)) lastAt.delete(k)
  }
  return true
}

/** 测试用：清空去重表（用例之间互不干扰） */
export function __resetDownloadGuard(): void {
  lastAt.clear()
}

/**
 * 探活产物文件：`null` = 可用（或判不了），字符串 = 人话失败原因。
 *
 * 只对**确定**的失败给结论：404/400/其它 4xx-5xx 才算失败；405（服务端不支持 HEAD）、
 * 网络层抛错一律返回 `null`（浏览器下载器会自己报错，这里不抢着下结论）。
 */
export async function probeArtifact(path: string): Promise<string | null> {
  if (typeof fetch !== 'function') return null
  let res: Response
  try {
    res = await fetch(artifactUrl(path, true), { method: 'HEAD' })
  } catch {
    return null
  }
  if (res.ok) return null
  if (res.status === 405) return null
  if (res.status === 404) return tGlobal('产物文件已不在磁盘（可能被清理或临时目录已回收）')
  if (res.status === 400) return tGlobal('产物路径不被允许读取')
  return tGlobal('服务端读取失败（HTTP {code}）', { code: res.status })
}

export type DownloadClickHandler = (
  e: Pick<ReactMouseEvent<HTMLAnchorElement>, 'preventDefault'>,
  path: string,
  name: string,
) => void

/**
 * 产物下载点击处理器：去重 → toast → 异步探活（失败补一条 error toast）。
 * 放行时**不 preventDefault**，让原生下载照常发生（同一手势，不会被拦）。
 */
export function useArtifactDownload(): DownloadClickHandler {
  const { toast } = useToast()
  return useCallback<DownloadClickHandler>(
    (e, path, name) => {
      if (!claimDownload(path)) {
        e.preventDefault()
        toast('info', tGlobal('已在下载'), tGlobal('{name} 刚开始下载，请稍候再试', { name }))
        return
      }
      toast('info', tGlobal('开始下载'), tGlobal('{name} → 浏览器下载目录', { name }))
      void probeArtifact(path).then((problem) => {
        if (problem) toast('error', tGlobal('下载失败'), tGlobal('{name}：{err}', { name, err: problem }))
      })
    },
    [toast],
  )
}

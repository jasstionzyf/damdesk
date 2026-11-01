/**
 * KI-9 / KI-10（机制层）：`<PathLink>` —— 界面上出现的磁盘路径的**唯一**渲染组件。
 *
 * 产品口径见 skill `soujpg-ui-patterns` §9.9 与 `damdesk-repo/references/path-reveal-matrix.md`：
 * 屏幕上出现的每个本机磁盘路径都必须可点，点了在系统文件管理器里打开/选中；
 * 两种语义分开 —— `mode="reveal"` =「在文件夹中显示」（资产级，选中文件）、
 * `mode="open"` =「打开文件夹」（目录级）。
 *
 * 形态门控：读 `capabilities.features.openInOS`（服务端算，容器形态 false）。
 * - true → 渲染 secondary 级按钮（§9.8：静止态就必须看得出可点，不用裸文字、不用 ink-dim 当按钮字色）+ 复制路径；
 * - false → **不渲染按钮**（灰按钮能点会被骂），改「复制路径」+ 人话原因（`featureReasons.openInOS`）。
 *
 * 失败语义交给服务端 `code`（前端不解析文案、也不自己做白名单）：
 * `path_not_allowed` / `path_missing` / `feature_disabled` / `bad_path` / `reveal_failed`。
 * 所有触点上的人话都在这里映射一次，页面不许再各写一套。
 */
import { useState } from 'react'
import { api } from '../lib/api'
import { can, reasonFor, useCapabilities } from '../lib/caps'
import { tGlobal, useI18n } from '../i18n'

export type PathLinkMode = 'reveal' | 'open'

/** 渲染形态：full = 全路径摊开（资产路径，默认）；compact = 折叠成「名 + 卷/父目录」（窄容器：侧栏库根） */
export type PathDisplay = 'full' | 'compact'

export interface PathLinkProps {
  /** 绝对路径；空/缺省 → 不渲染（老数据可能没有路径字段，不能崩） */
  path: string | null | undefined
  /** reveal（资产级，默认）/ open（目录级） */
  mode?: PathLinkMode
  /** 覆盖默认文案（默认「在文件夹中显示」/「打开文件夹」，两处触点必须同一个词） */
  label?: string
  /** 追加到最外层容器的样式（页面排版用，不改可点性） */
  className?: string
  /** 渲染形态，默认 `full`（零行为变更）。`compact` 只用于窄容器（侧栏库根）。 */
  display?: PathDisplay
  /**
   * compact 去重用的同列表路径（可选；`display="full"` 时忽略）。
   * 不做跨组件全局去重：谁渲染列表谁把列表传进来。basename 重名 → 自动多露一级父目录，
   * 最多露两级（再重名就接受同名，`title` 始终是全路径）。
   */
  siblings?: string[]
}

/** 路径拆解：卷（`D:` / `/`，相对路径为 ''）+ 卷下目录段 + 分隔符。 */
export interface PathParts {
  volume: string
  segs: string[]
  sep: string
}

/** `D:\素材\2024` → { volume:'D:', segs:['素材','2024'], sep:'\\' }；`/Volumes/SSD` → { volume:'/', segs:['Volumes','SSD'], sep:'/' } */
export function splitPath(raw: string): PathParts {
  const p = (raw ?? '').trim()
  const win = /^([A-Za-z]:)[\\/]?/.exec(p)
  if (win) {
    return { volume: win[1], segs: p.slice(win[0].length).split(/[\\/]+/).filter(Boolean), sep: '\\' }
  }
  if (p.startsWith('/')) return { volume: '/', segs: p.split('/').filter(Boolean), sep: '/' }
  return { volume: '', segs: p.split('/').filter(Boolean), sep: '/' }
}

/** compact 主行：从 basename 起逐级加父目录，直到与同列表其它路径不重名（cap 两级）。 */
export function compactLabel(path: string, siblings: string[] = []): string {
  const p = (path ?? '').trim()
  const { segs } = splitPath(p)
  if (segs.length === 0) return p // 根路径没有 basename → 原样显示（`/`、`D:\`）
  const self = p
  const others = (siblings ?? []).map((s) => (s ?? '').trim()).filter((s) => s && s !== self)
  const labelAt = (q: string, depth: number): string => {
    const { segs: qs, sep: qsep } = splitPath(q)
    return qs.slice(Math.max(0, qs.length - 1 - depth)).join(qsep)
  }
  const maxDepth = Math.min(2, segs.length - 1)
  for (let depth = 0; depth <= maxDepth; depth += 1) {
    const label = labelAt(p, depth)
    if (!others.some((o) => labelAt(o, depth) === label)) return label
  }
  return labelAt(p, maxDepth)
}

/**
 * compact 副行（弱化）：卷 + 一级父目录（POSIX 两级，贴合 `/Volumes/SSD/…` 习惯），更深的收成 `…`。
 * 没有父目录时只回卷本身（`/`、`D:\`），保证「库根仍可辨识路径身份」。
 */
export function compactHint(path: string): string {
  const { volume, segs, sep } = splitPath(path)
  const parents = segs.slice(0, Math.max(0, segs.length - 1))
  const rootOnly = volume === '/' ? '/' : volume ? `${volume}${sep}` : ''
  if (parents.length === 0) return rootOnly
  const span = volume && volume !== '/' ? 1 : 2
  const shown = parents.slice(0, span)
  const head =
    volume === '/'
      ? `/${shown.join('/')}`
      : volume
        ? `${volume}${sep}${shown.join(sep)}`
        : shown.join(sep)
  return parents.length > shown.length ? `${head}${sep}…` : head
}


/** 服务端 `code` → 人话（唯一映射点）。 */
/** 模块级纯函数 → tGlobal（读当前 locale）。 */
export function humanRevealError(e: unknown, gateReason = ''): string {
  const err = e as { code?: string; message?: string; status?: number } | null
  switch (err?.code) {
    case 'feature_disabled':
      return gateReason || tGlobal('当前版本无法在系统文件管理器中打开（文件不在本机）')
    case 'path_not_allowed':
      return tGlobal('该路径不在允许打开的范围内（仅库根 / 导入源 / 导出目标 / 回收站原路径）')
    case 'path_missing':
      return tGlobal('目标不存在或已断开（可移动盘？）')
    case 'bad_path':
      return tGlobal('路径无效（需要绝对路径，且不能含 ..）')
    case 'reveal_failed':
      return tGlobal('没能在系统文件管理器中打开（找不到 explorer / open / xdg-open）')
    default:
      return err?.message || tGlobal('打开失败')
  }
}

const BTN =
  'inline-flex h-8 shrink-0 items-center gap-1 rounded border border-line bg-panel-2 px-3 text-xs text-ink ' +
  'hover:border-accent hover:text-accent focus-visible:outline-none focus-visible:ring-2 ' +
  'focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-bg disabled:opacity-50'

/** 剪贴板：优先 Clipboard API，老 WebView 兜底 execCommand（都不行就返回 false，不假装成功）。 */
async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch {
    /* 权限被拒 → 走兜底 */
  }
  try {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.setAttribute('readonly', '')
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.select()
    const ok = document.execCommand?.('copy') ?? false
    document.body.removeChild(ta)
    return ok
  } catch {
    return false
  }
}

export function PathLink({
  path,
  mode = 'reveal',
  label,
  className = '',
  display = 'full',
  siblings,
}: PathLinkProps) {
  const { t } = useI18n()
  const caps = useCapabilities()
  const canOpen = can(caps, 'openInOS')
  const gateReason = reasonFor(caps, 'openInOS')
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState('')
  const [copied, setCopied] = useState(false)

  const p = (path ?? '').trim()
  if (!p) return null // 老数据无路径 → 不渲染，也不崩

  const text = label ?? (mode === 'reveal' ? t('在文件夹中显示') : t('打开文件夹'))
  const compact = display === 'compact'
  const hint = compact ? compactHint(p) : ''

  const onOpen = async () => {
    setErr('')
    setBusy(true)
    try {
      await api.osReveal(p, mode)
    } catch (e) {
      setErr(humanRevealError(e, gateReason))
    } finally {
      setBusy(false)
    }
  }

  const onCopy = async () => {
    const ok = await copyText(p)
    setCopied(ok)
    setErr(ok ? '' : t('复制失败（剪贴板不可用），请手动选中路径复制'))
    if (ok) window.setTimeout(() => setCopied(false), 1500)
  }

  /** 「打开 / 复制」两个动作的载体（形态门控只在 canOpen 上，compact 不改语义） */
  const actions = (
    <span className="inline-flex shrink-0 items-center gap-2" data-testid="pathlink-actions">
      {canOpen && (
        <button
          type="button"
          data-testid="pathlink-open"
          className={BTN}
          title={text}
          onClick={onOpen}
          disabled={busy}
        >
          {busy ? t('正在打开…') : text}
        </button>
      )}
      <button
        type="button"
        data-testid="pathlink-copy"
        className={BTN}
        title={t('复制路径')}
        onClick={onCopy}
      >
        {copied ? t('已复制') : t('复制路径')}
      </button>
    </span>
  )

  return (
    <span
      data-testid="pathlink"
      data-gate={canOpen ? 'open' : 'copy'}
      data-mode={mode}
      data-display={display}
      className={`inline-flex min-w-0 flex-col gap-1 ${className}`}
    >
      {compact ? (
        /* 窄容器：主行 = 去重后的名（单行截断），副行 = 卷/父目录（弱化）；title 永远是全路径 */
        <span className="flex min-w-0 flex-col gap-1">
          <span className="flex min-w-0 flex-col">
            <span
              data-testid="pathlink-path"
              title={p}
              className="min-w-0 truncate text-xs font-medium text-ink"
            >
              {compactLabel(p, siblings)}
            </span>
            {hint && (
              <span
                data-testid="pathlink-hint"
                title={p}
                className="min-w-0 truncate font-mono text-[11px] text-ink-dim"
              >
                {hint}
              </span>
            )}
          </span>
          {actions}
        </span>
      ) : (
        <span className="inline-flex min-w-0 items-start gap-2">
          <span
            data-testid="pathlink-path"
            title={p}
            className="min-w-0 flex-1 break-all font-mono text-xs text-ink-dim"
          >
            {p}
          </span>
          {actions}
        </span>
      )}
      {!canOpen && (
        <span data-testid="pathlink-reason" className="text-xs text-ink-dim">
          {gateReason || t('文件在服务器上，无法在本机文件管理器中打开')}
        </span>
      )}
      {err && (
        <span data-testid="pathlink-error" role="alert" className="text-xs text-ink">
          {err}
        </span>
      )}
    </span>
  )
}

export default PathLink

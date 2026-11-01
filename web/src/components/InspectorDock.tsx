/**
 * DOCKED（卡 t_ac7de3d5）：检查器右侧**非模态**抽屉。
 *
 * 用户裁决：检查器不再常驻占第三栏（把宽屏主区从 512 挤到「中文竖排」），改成与
 * packs Drawer / export Dialog 同一套「浮层」模式，但**非模态**：不遮罩、不锁背景，
 * 选中即滑出、能边看图边看详情。
 *
 * 三条口径（两处挂载点共用，别再各写一套）：
 *  1. URL 真源：`?drawer=inspector`（`useOverlay`）→ 刷新/深链/back 都能恢复。
 *     刻意**不清 `ids`**：选择集是页面自己的 URL 真源（Gallery 写-through），
 *     浮层关闭时替它删 ids 会和页面的 write-through 打架。
 *  2. 自动弹出：有选中且本会话没手动关过 → 自动开。
 *  3. 关闭记忆：用户点 ✕ / 按 Esc 关掉后写 sessionStorage（`dam.inspector.autoClose:<key>`），
 *     本会话内不再自动弹；用户再手动点工具条「详情」打开时清掉该标记（此后恢复自动弹）。
 *
 * 面板本身（宽度/边框/标题栏/滚动容器）由 `lib/modal.tsx` 的 `<DockedPanel>` 出，
 * `<Inspector variant="docked">` 只出内容 —— 避免双层标题栏与二级滚动。
 */
import { useCallback, useEffect, useRef } from 'react'
import { useI18n } from '../i18n'
import { DockedPanel, useOverlay } from '../lib/modal'
import { Inspector } from './Inspector'

/** 本会话「用户手动关掉过」的标记前缀（sessionStorage：关掉窗口即忘） */
const AUTO_CLOSE_PREFIX = 'dam.inspector.autoClose:'

function readAutoClose(key: string): boolean {
  try {
    return sessionStorage.getItem(AUTO_CLOSE_PREFIX + key) === '1'
  } catch {
    // 存储不可用（隐私模式 / 配额满）：只靠组件内 ref 兜底（见 useInspectorDock）
    return false
  }
}

function writeAutoClose(key: string, on: boolean) {
  try {
    if (on) sessionStorage.setItem(AUTO_CLOSE_PREFIX + key, '1')
    else sessionStorage.removeItem(AUTO_CLOSE_PREFIX + key)
  } catch {
    // 存储不可用：ref 兜底已生效（保证「关得掉」，不是静默把功能弄坏）
  }
}

export interface InspectorDockState {
  open: boolean
  /** 用户主动关闭（✕ / Esc）：记「本会话不再自动弹」 */
  close: () => void
  /** 工具条「详情」按钮：开 ↔ 关；手动打开时清掉自动弹出抑制标记 */
  toggle: () => void
}

/**
 * 抽屉开关状态（URL + 自动弹出 + 会话记忆）。
 * 由页面调用后把 `open` / `close` 传给 `<InspectorDock>`，`toggle` 放进工具条的「详情」按钮。
 */
export function useInspectorDock({
  autoOpenKey,
  hasSelection,
}: {
  /**
   * 会话记忆键前缀。KI-16 #7 之后只剩图库一个浏览面（智能文件夹并入图库，
   * 不再有第二个 `autoOpenKey`）；参数保留是为了别处（搜索页等）各自独立。
   */
  autoOpenKey: string
  /** 当前是否有可展示的选中（单选一张 / 多选批量） */
  hasSelection: boolean
}): InspectorDockState {
  const { open, openOverlay, closeOverlay } = useOverlay('drawer', 'inspector')
  /**
   * 「本次挂载内用户已关掉」的兜底标记：sessionStorage 不可用时也能关掉（否则关一次立刻被自动弹出拉回）。
   * sessionStorage 负责跨刷新/跨挂载的记忆。
   */
  const closedRef = useRef(false)

  const close = useCallback(() => {
    closedRef.current = true
    writeAutoClose(autoOpenKey, true)
    closeOverlay()
  }, [autoOpenKey, closeOverlay])

  const toggle = useCallback(() => {
    if (open) {
      close()
      return
    }
    // 手动打开 = 用户明确要它 → 清掉抑制标记，之后换图继续自动弹
    closedRef.current = false
    writeAutoClose(autoOpenKey, false)
    openOverlay()
  }, [open, close, openOverlay, autoOpenKey])

  // 选中即自动滑出（本会话手动关过就不再弹）
  useEffect(() => {
    if (!hasSelection || open) return
    if (closedRef.current || readAutoClose(autoOpenKey)) return
    openOverlay()
  }, [hasSelection, open, autoOpenKey, openOverlay])

  return { open, close, toggle }
}

type InspectorProps = Omit<Parameters<typeof Inspector>[0], 'variant'>

export interface InspectorDockProps extends InspectorProps {
  open: boolean
  onClose: () => void
  /** 上层浮层（Lightbox / Dialog / 快捷键帮助 / 确认框）打开时挂起 Esc */
  suspended?: boolean
}

/** 检查器抽屉（只在「打开 且 有选中」时存在；未选中不渲染空面板，也不留竖条） */
export function InspectorDock({ open, onClose, suspended, ...inspectorProps }: InspectorDockProps) {
  const { t } = useI18n()
  const isBatch = inspectorProps.batch.length > 1
  const hasSelection = !!inspectorProps.asset || isBatch
  return (
    <DockedPanel
      open={open && hasSelection}
      onClose={onClose}
      title={isBatch ? t('批量编辑') : t('检查器')}
      subtitle={isBatch ? t('已选 {n} 张', { n: inspectorProps.batch.length }) : undefined}
      testId="inspector-dock"
      suspended={suspended}
    >
      <Inspector variant="docked" {...inspectorProps} />
    </DockedPanel>
  )
}

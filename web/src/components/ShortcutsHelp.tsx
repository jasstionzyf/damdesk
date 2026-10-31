import { useEffect } from 'react'
import { ModalCloseButton, useModalDismiss } from '../lib/modal'
import { useI18n } from '../i18n'

/** E2 快捷键速查面板（? 打开；Esc / ✕ / 点击遮罩关闭） */
export function ShortcutsHelp({
  open,
  onClose,
  context = 'library',
}: {
  open: boolean
  onClose: () => void
  /** library = 图库网格；lightbox = 全屏预览 */
  context?: 'library' | 'lightbox'
}) {
  // 统一关闭语义（UI-DIALOG）：Esc 走公共栈（叠在 Lightbox 上时一次只关这一层）
  const { overlayProps, contentRef } = useModalDismiss({ open, onClose, overlayTestId: 'shortcuts-overlay' })
  const { t } = useI18n()

  // ? 同键切换关闭（Esc 已由 useModalDismiss 处理）
  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === '?') {
        e.preventDefault()
        e.stopPropagation()
        onClose()
      }
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open, onClose])

  if (!open) return null

  const libraryRows: [string, string][] = [
    ['/', t('聚焦搜索框（当前视图内按文件名/标题快速筛选）')],
    ['← / →', t('移动游标（单选）')],
    ['Shift + ← / →', t('从锚点范围扩展选择')],
    ['Cmd/Ctrl + A', t('全选当前视图')],
    [t('Cmd/Ctrl + 单击'), t('加选 / 取消加选')],
    [t('Shift + 单击'), t('范围选（锚点 → 目标）')],
    ['Enter', t('打开全屏预览（Lightbox）')],
    ['X', t('标记为不要 / 取消标记（选择集）')],
    ['E', t('导出选择集')],
    ['Delete / Backspace', t('删除选择集（移入回收站，二次确认后可撤销）')],
    ['Esc', t('逐层关闭：浮层 → 检查器抽屉 → 库面板 → 清空选择')],
    ['?', t('显示 / 隐藏本面板')],
  ]

  // E4 拖拽语义（不属于键位，单独一节展示）
  const dragRows: [string, string][] = [
    [t('拖图 → 左侧目录'), t('移动到该目录（磁盘 rename + absPath 同步）')],
    [t('拖图 → 左侧标签'), t('给全部拖入图片打上该标签')],
    [t('拖图 → 底部导出区'), t('打开导出对话框（拖已选项 = 整个选择集）')],
  ]

  const lightboxRows: [string, string][] = [
    ['← / →', t('上一张 / 下一张')],
    [t('Z 或单击图片'), t('切换适应屏幕 / 100% 像素检视')],
    ['?', t('显示 / 隐藏本面板')],
    ['Esc', t('退出全屏预览')],
  ]

  const rows = context === 'lightbox' ? [...lightboxRows, ...libraryRows.slice(0, 2)] : libraryRows

  return (
    <div
      {...overlayProps}
      className="fixed inset-0 z-[70] flex items-center justify-center bg-black/60 p-6"
      role="dialog"
      aria-label={t('键盘快捷键')}
    >
      <div
        ref={contentRef}
        className="max-h-[80vh] w-[520px] overflow-y-auto rounded-lg border border-line bg-panel p-5 shadow-2xl"
        data-testid="shortcuts-help"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between">
          <h2 className="text-sm font-semibold">
            {t('键盘快捷键')}<span className="ml-2 text-xs text-ink-dim">{context === 'lightbox' ? t('全屏预览') : t('图库网格')}</span>
          </h2>
          <ModalCloseButton onClose={onClose} label={t('关闭快捷键帮助')} />
        </div>
        <dl className="mt-4 space-y-1.5">
          {rows.map(([k, v], i) => (
            <div key={`${k}-${i}`} className="flex items-baseline gap-3 text-xs">
              <dt className="w-40 shrink-0">
                <kbd className="rounded border border-line bg-panel-2 px-1.5 py-0.5 font-mono text-[11px]">{k}</kbd>
              </dt>
              <dd className="text-ink-dim">{v}</dd>
            </div>
          ))}
        </dl>
        {context === 'library' && (
          <>
            <h3 className="mt-5 text-xs font-semibold text-ink-dim">{t('拖拽语义（E4）')}</h3>
            <dl className="mt-2 space-y-1.5">
              {dragRows.map(([k, v]) => (
                <div key={k} className="flex items-baseline gap-3 text-xs">
                  <dt className="w-40 shrink-0 text-ink-dim">{k}</dt>
                  <dd className="text-ink-dim">{v}</dd>
                </div>
              ))}
            </dl>
          </>
        )}
      </div>
    </div>
  )
}

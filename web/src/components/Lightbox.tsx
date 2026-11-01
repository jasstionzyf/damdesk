import { useCallback, useEffect, useRef, useState } from 'react'
import { useI18n } from '../i18n'
import { api, formatSize, type AssetView } from '../lib/api'
import { Button } from './Button'

export interface LightboxProps {
  /** 当前视图（与网格同序；翻页即在此数组内移动） */
  items: AssetView[]
  /** 当前下标（由 Grid 的 activeId 推导，双向同步） */
  index: number
  onIndex: (i: number) => void
  onClose: () => void
  onHelp: () => void
  /** 上层浮层（快捷键帮助）打开时挂起键位处理，避免 Esc 同时关掉两层 */
  suspended?: boolean
}

/**
 * E2 Lightbox 全屏预览（Lightroom 惯例键位）。
 * KI-11/KI-12：只保留「大图 + 翻页 + 缩放」职责，不承载编辑 —— 打星已随评分能力一起移除，
 * 拒选/标签/色标等操作一律回检查器抽屉（一个能力一个入口）。
 */
export function Lightbox({
  items,
  index,
  onIndex,
  onClose,
  onHelp,
  suspended = false,
}: LightboxProps) {
  const { t } = useI18n()
  const [zoom, setZoom] = useState(false)
  const [loaded, setLoaded] = useState(false)
  const [hint, setHint] = useState<string | null>(null)
  const scroller = useRef<HTMLDivElement | null>(null)
  const hintTimer = useRef<number | null>(null)

  const asset: AssetView | undefined = items[index]

  const flashHint = useCallback((msg: string) => {
    setHint(msg)
    if (hintTimer.current !== null) window.clearTimeout(hintTimer.current)
    hintTimer.current = window.setTimeout(() => setHint(null), 1200)
  }, [])

  // 换图：重置缩放 / 加载态
  useEffect(() => {
    setZoom(false)
    setLoaded(false)
  }, [asset?.assetId])

  // 相邻预加载（翻页不闪白）
  useEffect(() => {
    for (const d of [-1, 1]) {
      const a = items[index + d]
      if (a) {
        const img = new Image()
        img.src = api.thumbUrl(a.assetId, 1024)
      }
    }
  }, [items, index])

  const go = useCallback(
    (delta: number) => {
      const next = index + delta
      if (next < 0) return flashHint(t('已是第一张'))
      if (next >= items.length) return flashHint(t('已是最后一张'))
      onIndex(next)
    },
    [index, items.length, onIndex, flashHint],
  )

  const toggleZoom = useCallback(() => {
    setZoom((z) => {
      const nz = !z
      // 进入 100% 时把视口居中到图片中心
      window.setTimeout(() => {
        const el = scroller.current
        if (!el || !nz) return
        el.scrollLeft = Math.max(0, (el.scrollWidth - el.clientWidth) / 2)
        el.scrollTop = Math.max(0, (el.scrollHeight - el.clientHeight) / 2)
      }, 30)
      return nz
    })
  }, [])

  // 键位（capture，避免 Esc/方向键穿透到 Grid 的全局处理器）
  useEffect(() => {
    if (suspended) return
    const onKey = (e: KeyboardEvent) => {
      const k = e.key
      if (k === 'Escape') {
        e.preventDefault()
        e.stopPropagation()
        onClose()
        return
      }
      if (k === '?' || (k === '/' && e.shiftKey)) {
        e.preventDefault()
        e.stopPropagation()
        onHelp()
        return
      }
      if (k === 'ArrowLeft' || k === 'ArrowRight' || k === 'ArrowUp' || k === 'ArrowDown') {
        e.preventDefault()
        e.stopPropagation()
        go(k === 'ArrowLeft' || k === 'ArrowUp' ? -1 : 1)
        return
      }
      if (k === 'z' || k === 'Z') {
        e.preventDefault()
        e.stopPropagation()
        toggleZoom()
        return
      }
      if (k === 'Home') {
        e.preventDefault()
        onIndex(0)
        return
      }
      if (k === 'End') {
        e.preventDefault()
        onIndex(items.length - 1)
        return
      }
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [go, toggleZoom, onClose, onHelp, onIndex, items.length, suspended])

  const rejected = asset?.approvalStatus === 'rejected'

  // DOCKED：I 信息面板已删（详情只在检查器抽屉里看，一个能力一个入口）。

  if (!asset) return null

  return (
    <div className="fixed inset-0 z-50 flex flex-col bg-black/95" role="dialog" aria-label={t('全屏预览')} data-testid="lightbox">
      {/* 顶栏 */}
      <div className="flex h-12 shrink-0 items-center gap-3 border-b border-white/10 px-4 text-xs text-white/80">
        <span className="font-mono" data-testid="lightbox-counter">
          {index + 1} / {items.length}
        </span>
        <span className="max-w-[40ch] truncate font-medium text-white" data-testid="lightbox-filename">
          {asset.fileName}
        </span>
        <span className="text-white/50">
          {asset.width && asset.height ? `${asset.width}×${asset.height} · ` : ''}
          {formatSize(asset.fileSz)}
        </span>
        {rejected && (
          <span className="rounded bg-red-600/80 px-1.5 py-0.5 text-[10px] text-white">{t('已拒选')}</span>
        )}
        <span className="ml-auto flex items-center gap-2">
          <a
            href={api.originalUrl(asset.assetId)}
            target="_blank"
            rel="noreferrer"
            className="rounded border border-white/20 px-2 py-0.5 text-[11px] hover:border-white/50"
          >
            {t('打开原图')}
          </a>
          <Button
            type="button"
            title={t('键盘快捷键（?）')}
            onClick={onHelp}
            variant="secondary"
            size="none"
            className="px-2 py-0.5 text-[11px]"
          >
            ？
          </Button>
          <Button
            type="button"
            title={t('退出（Esc）')}
            onClick={onClose}
            variant="secondary"
            size="none"
            className="px-2 py-0.5 text-[11px]"
          >
            {t('退出')}
          </Button>
        </span>
      </div>

      <div className="flex min-h-0 flex-1">
        {/* 图片区 */}
        <div className="relative flex min-w-0 flex-1 items-center justify-center">
          <Button
            type="button"
            aria-label={t('上一张')}
            data-testid="lightbox-prev"
            onClick={() => go(-1)}
            variant="secondary"
            size="none"
            className="absolute left-3 z-10 rounded-full px-3 py-2"
          >
            ‹
          </Button>
          {zoom ? (
            <div ref={scroller} className="h-full w-full overflow-auto" data-testid="lightbox-zoom-area">
              <img
                src={api.originalUrl(asset.assetId)}
                alt={asset.fileName}
                data-testid="lightbox-image"
                className="max-w-none cursor-zoom-out"
                onClick={toggleZoom}
              />
            </div>
          ) : (
            <img
              src={api.thumbUrl(asset.assetId, 1024)}
              alt={asset.fileName}
              data-testid="lightbox-image"
              className="max-h-full max-w-full cursor-zoom-in object-contain p-4"
              onClick={toggleZoom}
              onLoad={() => setLoaded(true)}
            />
          )}
          {!loaded && !zoom && (
            <span className="absolute bottom-4 left-1/2 -translate-x-1/2 text-xs text-white/50">{t('加载中…')}</span>
          )}
          <span className="absolute bottom-4 right-4 rounded bg-black/50 px-2 py-1 text-[11px] text-white/70">
            {zoom ? t('100% 像素检视（Z / 单击还原）') : t('适应屏幕（Z / 单击放大）')}
          </span>
          {hint && (
            <span
              className="absolute top-4 left-1/2 -translate-x-1/2 rounded bg-black/70 px-3 py-1 text-xs text-white"
              data-testid="lightbox-hint"
            >
              {hint}
            </span>
          )}
          <Button
            type="button"
            aria-label={t('下一张')}
            data-testid="lightbox-next"
            onClick={() => go(1)}
            variant="secondary"
            size="none"
            className="absolute right-3 z-10 rounded-full px-3 py-2"
          >
            ›
          </Button>
        </div>
      </div>

      {/* 底栏键位提示（KI-12：Lightbox 只管大图 + 翻页，编辑回检查器） */}
      <div className="flex h-9 shrink-0 items-center gap-4 border-t border-white/10 px-4 text-[11px] text-white/50">
        <span>{t('← / → 翻页')}</span>
        <span>{t('Z 像素检视')}</span>
        <span>{t('? 快捷键')}</span>
        <span>{t('Esc 退出')}</span>
      </div>
    </div>
  )
}

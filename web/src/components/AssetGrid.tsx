import { forwardRef, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { VirtuosoGrid } from 'react-virtuoso'
import { api, formatSize, type AssetView } from '../lib/api'
import type { FilterChip } from './LibraryTree'
import { DEFAULT_THUMB } from './DensityControl'
import { FilterBanner } from './FilterBanner'
import { useI18n } from '../i18n'

export interface PickMods {
  shift: boolean
  meta: boolean
}

/**
 * 中栏：工具条（计数 + 筛选芯片 + 动作区）+ 虚拟化网格（万级缩略图只渲染视口 DOM）。
 * E1：react-virtuoso 虚拟化 + 触底自动加载 + 缩略图渐进加载（骨架 → 256 低清 → hover 1024 高清）
 *     + 筛选芯片可视化 + 滚动位置快照恢复。
 * E2：多选（单击 / cmd|ctrl+单击加选 / shift+单击范围选）+ 双击进 Lightbox。
 * E4：卡片拖拽（拖已选=整个选择集）。
 */

/* ---- Virtuoso 自定义容器（模块级定义，保持引用稳定：内联重建会让内部重挂载，快速滚动时形成渲染风暴） ---- */

const Scroller = forwardRef<HTMLDivElement, React.ComponentProps<'div'>>(function Scroller(
  { style, ...rest },
  ref,
) {
  return <div ref={ref} {...rest} style={style} className="min-h-0 flex-1 overflow-x-hidden overflow-y-auto" />
})

const List = forwardRef<HTMLDivElement, React.ComponentProps<'div'>>(function List(
  { style, ...rest },
  ref,
) {
  // 列数由容器宽度算（auto-fill）；最小列宽来自密度档 --thumb-min（UI-LAYOUT / 卡 task）
  return (
    <div
      ref={ref}
      {...rest}
      style={style}
      className="dam-grid grid grid-cols-[repeat(auto-fill,minmax(var(--thumb-min,170px),1fr))] gap-3 p-3"
    />
  )
})

const Item = forwardRef<HTMLDivElement, React.ComponentProps<'div'>>(function Item(
  { style, ...rest },
  ref,
) {
  return <div ref={ref} {...rest} style={style} />
})

/** Footer 需要的动态状态（经 VirtuosoGrid context 传入，避免重建 components 导致内部重挂载） */
interface FooterCtx {
  loading: boolean
  hasMore: boolean
  loaded: number
  total: number
  onLoadMore: () => void
}

function GridFooter({ context }: { context: FooterCtx }) {
  const { loading, hasMore, loaded, total, onLoadMore } = context
  const { t } = useI18n()
  return (
    <div className="col-span-full mt-2 flex justify-center pb-2">
      {loading && <span className="text-xs text-ink-dim">{t('加载中…')}</span>}
      {!loading && hasMore && (
        <button
          type="button"
          className="rounded border border-line px-4 py-1.5 text-xs text-ink-dim hover:border-accent hover:text-accent"
          onClick={onLoadMore}
        >
          {t('加载更多（{loaded} / {total}）', { loaded, total })}
        </button>
      )}
      {!loading && !hasMore && loaded > 0 && (
        <span className="text-xs text-ink-dim">{t('已全部加载（{total} 张）', { total })}</span>
      )}
      {!loading && loaded === 0 && (
        <span className="text-xs text-ink-dim">{t('当前筛选条件下没有图片')}</span>
      )}
    </div>
  )
}

const gridComponents = { Scroller, List, Item, Footer: GridFooter }

/** E5 导入即图库：网格项 = 真实资产 或 待导入占位（半透明，缩略图就绪即点亮） */
export type GridItem = { kind: 'asset'; asset: AssetView } | { kind: 'pending'; name: string }

const itemKey = (_: number, it: GridItem) =>
  it.kind === 'asset' ? `a-${it.asset.assetId}` : `p-${it.name}`

function PendingCard({ name }: { name: string }) {
  const { t } = useI18n()
  return (
    <div
      data-testid="pending-card"
      data-name={name}
      title={t('{name} · 导入中，缩略图就绪后自动点亮', { name })}
      className="flex aspect-square flex-col items-center justify-center gap-1.5 rounded-md border border-dashed border-line bg-panel-2/50 opacity-60"
    >
      <span className="h-5 w-5 animate-spin rounded-full border-2 border-line border-t-accent" />
      <span className="max-w-full truncate px-1 text-[10px] text-ink-dim">{name}</span>
      <span className="text-[10px] text-ink-dim/70">{t('导入中…')}</span>
    </div>
  )
}

/* ---- 筛选芯片（当前条件可视化 + 一键移除）已抽到 FilterBanner（P1-2：醒目横幅） ---- */

/* ---- 资产卡（E1 渐进加载：骨架 → 256，hover 换 1024 高清；E2 选择态；E4 拖拽） ----
 * 导出以便其他视图（如搜索页结果）复用同一张卡：卡片行为必须与图库一致。 */
export function AssetCard({
  asset,
  isSel,
  isActive,
  selectedIds,
  onPick,
  onOpen,
  onDragAssets,
  onDragEnd,
  onContextMenu,
  dirty,
}: {
  asset: AssetView
  isSel: boolean
  isActive: boolean
  selectedIds: number[]
  onPick: (a: AssetView, mods: PickMods) => void
  onOpen: (a: AssetView) => void
  onDragAssets?: (ids: number[]) => void
  onDragEnd?: () => void
  /** KI-10：右键菜单（图库用「在文件夹中显示」；不传则保留浏览器默认菜单） */
  onContextMenu?: (a: AssetView, e: React.MouseEvent) => void
  /** KI-12：搜索页对这张卡做过就地修改（标签/色标/拒选/移动）→「已修改」角标；结果集不重算的可见补偿 */
  dirty?: boolean
}) {
  const { t } = useI18n()
  const [hi, setHi] = useState(false)
  const [loaded, setLoaded] = useState(false)
  const rejected = asset.approvalStatus === 'rejected'
  // D3「无法预览」：两种来源都要显示角标 ——
  //  ① 库里就没有正尺寸（导入时解码失败 → width/height 为 0）；
  //  ② 库里有尺寸但缩略图这次没取到（文件后来被写坏/被删：接口降级成占位图后
  //     仍会 200，这里兜住真正的网络/解析失败）。
  // 失联（missing=1）单独有角标，不重复报「无法预览」—— 失联是文件没了，语义不同。
  const [thumbErr, setThumbErr] = useState(false)
  const noPreview = asset.missing !== 1 && (thumbErr || !(asset.width && asset.height))
  const noPreviewReason =
    asset.width && asset.height
      ? t('缩略图加载失败（源文件可能已损坏或被移走）')
      : t('导入时解码失败：文件内容不是有效图片或已损坏')
  return (
    <button
      type="button"
      data-testid={`asset-card-${asset.assetId}`}
      data-selected={isSel ? '1' : '0'}
      draggable={!!onDragAssets}
      onClick={(e) => onPick(asset, { shift: e.shiftKey, meta: e.metaKey || e.ctrlKey })}
      onDoubleClick={() => onOpen(asset)}
      onContextMenu={
        onContextMenu
          ? (e) => {
              e.preventDefault()
              onContextMenu(asset, e)
            }
          : undefined
      }
      onMouseEnter={() => setHi(true)}
      onMouseLeave={() => setHi(false)}
      onDragStart={(e) => {
        if (!onDragAssets) return
        // 拖已选项 = 整个选择集；拖未选项 = 仅这一张（Lightroom 惯例）
        const ids = isSel ? [...selectedIds] : [asset.assetId]
        e.dataTransfer.setData('text/plain', ids.join(','))
        e.dataTransfer.effectAllowed = 'copyMove'
        onDragAssets(ids)
      }}
      onDragEnd={() => onDragEnd?.()}
      title={`${asset.fileName}${
        noPreview ? t('\n⚠ 无法预览：{reason}', { reason: noPreviewReason }) : ''
      }${t('\n双击全屏预览（Enter 亦可）· 悬停加载高清\n可拖到左侧目录=移动 / 标签=打标 / 导出区=导出')}`}
      className={`group overflow-hidden rounded-md border bg-panel text-left transition-colors ${
        isSel ? 'border-accent ring-1 ring-accent' : 'border-line hover:border-ink-dim'
      } ${isActive ? 'outline outline-1 outline-offset-1 outline-accent/60' : ''}`}
    >
      <div className="relative aspect-4/3 w-full overflow-hidden bg-panel-2">
        {!loaded && <div className="absolute inset-0 animate-pulse bg-panel-2" aria-hidden />}
        <img
          src={api.thumbUrl(asset.assetId, hi ? 1024 : 256)}
          alt={asset.fileName}
          loading="lazy"
          decoding="async"
          onLoad={() => setLoaded(true)}
          onError={() => {
            // D3：接口已降级成 200 占位图，走到这里说明连占位图都没拿到
            // （断网/后端退出）→ 也要给标识，别把骨架屏挂死
            setThumbErr(true)
            setLoaded(true)
          }}
          className={`h-full w-full object-cover transition-opacity duration-150 ${
            loaded ? 'opacity-100' : 'opacity-0'
          } ${rejected ? 'grayscale' : ''} ${rejected && loaded ? 'opacity-40' : ''}`}
        />
        {asset.missing === 1 && (
          <span className="absolute top-1.5 left-1.5 rounded bg-amber-600/90 px-1.5 py-0.5 text-[10px] text-black">
            {t('失联')}
          </span>
        )}
        {noPreview && (
          <span
            data-testid={`asset-nopreview-${asset.assetId}`}
            title={noPreviewReason}
            className={`absolute rounded bg-ink-dim/90 px-1.5 py-0.5 text-[10px] text-black ${
              asset.missing === 1 ? 'top-1.5 left-16' : 'top-1.5 left-1.5'
            }`}
          >
            {t('无法预览')}
          </span>
        )}
        {rejected && (
          <span className="absolute top-1.5 right-1.5 rounded bg-red-600/90 px-1.5 py-0.5 text-[10px] text-white">
            {t('已拒选')}
          </span>
        )}
        {isSel && (
          <span className="absolute bottom-1.5 left-1.5 flex h-4 w-4 items-center justify-center rounded-full bg-accent text-[10px] font-bold text-white">
            ✓
          </span>
        )}
        {dirty && (
          <span
            data-testid={`asset-dirty-${asset.assetId}`}
            title={t('这张卡在当前结果里被就地修改过（搜索结果不自动重算；重跑搜索即还原最新数据）')}
            className="absolute bottom-1.5 right-1.5 rounded bg-accent/90 px-1.5 py-0.5 text-[10px] text-white"
          >
            {t('已修改')}
          </span>
        )}
      </div>
      <div className="px-2 py-1.5">
        <p className="truncate text-xs">{asset.fileName}</p>
        <p className="mt-0.5 truncate text-[10px] text-ink-dim">
          {asset.width && asset.height ? `${asset.width}×${asset.height} · ` : ''}
          {formatSize(asset.fileSz)}
        </p>
      </div>
    </button>
  )
}

/* ---- 主组件 ---- */

export function AssetGrid({
  assets,
  total,
  selectedIds,
  activeId,
  onPick,
  onOpen,
  hasMore,
  loading,
  onLoadMore,
  toolbar,
  searchSlot,
  resetSignal,
  emptyState,
  pendingState,
  onDragAssets,
  onDragEnd,
  chips,
  pendingNames,
  onRemoveChip,
  onClearFilter,
  restoreScrollTop,
  onScrollTop,
  thumbPx = DEFAULT_THUMB,
  onAssetContextMenu,
}: {
  assets: AssetView[]
  total: number
  selectedIds: number[]
  activeId: number | null
  onPick: (a: AssetView, mods: PickMods) => void
  onOpen: (a: AssetView) => void
  hasMore: boolean
  loading: boolean
  onLoadMore: () => void
  /** 工具条右侧动作区（拒选 / 导出 / 快捷键） */
  toolbar?: React.ReactNode
  /** 工具条搜索框（/ 聚焦） */
  searchSlot?: React.ReactNode
  /**
   * 值变化即把列表滚回顶部（不清组件树）。
   * 取代「父级 key 变化整块重建」：重建会把工具条/搜索框一起重建 → 输入框失焦，
   * 用户「打完等结果、再接着打字」时输入全丢（KI-32）。
   */
  resetSignal?: string
  /** 空结果时渲染在工具条**下方**（工具条/搜索框保持挂载 —— 卸载会让用户「搜不到就没法改词」） */
  emptyState?: React.ReactNode
  /** 查询在途（尚无结果）时渲染在工具条下方 */
  pendingState?: React.ReactNode
  /** E4 拖拽开始（ids = 拖已选项则整个选择集，否则单张）；置 null 表示未启用拖拽 */
  onDragAssets?: (ids: number[]) => void
  /** E4 拖拽结束（drop 或取消），外层清理高亮/浮层 */
  onDragEnd?: () => void
  /** E1 当前筛选芯片（有值时渲染在工具条下方，一键移除） */
  chips?: FilterChip[]
  onRemoveChip?: (field: FilterChip['field']) => void
  onClearFilter?: () => void
  /** E5 导入即图库：导入中未落库的文件名（渲染为网格顶部半透明占位卡，逐个点亮） */
  pendingNames?: string[]
  /** E1 返回图库页时恢复的滚动位置（px；null=不恢复，仅挂载后首帧布局生效） */
  restoreScrollTop?: number | null
  /** E1 持续上报滚动位置（供离页保存快照） */
  onScrollTop?: (top: number) => void
  /** 网格密度：缩略图最小列宽 px（来自 URL 的 thumb，见 DensityControl） */
  thumbPx?: number
  /** KI-10：卡片右键菜单（图库用「在文件夹中显示」；不传则保留浏览器默认菜单） */
  onAssetContextMenu?: (a: AssetView, e: React.MouseEvent) => void
}) {
  const { t } = useI18n()
  const selected = new Set(selectedIds)
  const endReached = useCallback(() => {
    if (hasMore && !loading) onLoadMore()
  }, [hasMore, loading, onLoadMore])

  // E1 滚动位置：持续上报 + 快照恢复（布局未就绪 scrollTop 会被 clamp，逐帧重试直到生效）
  const restored = useRef(false)
  const scrollerEl = useRef<HTMLElement | null>(null)
  const reportScroll = useRef(onScrollTop)
  reportScroll.current = onScrollTop
  const scrollerRef = useCallback((el: HTMLElement | null) => {
    scrollerEl.current = el
    if (el && reportScroll.current) {
      const cb = reportScroll.current
      el.addEventListener('scroll', () => cb(el.scrollTop), { passive: true })
    }
  }, [])
  useEffect(() => {
    const el = scrollerEl.current
    const target = restoreScrollTop ?? null
    if (!el || target == null || restored.current || assets.length === 0) return
    restored.current = true
    let tries = 0
    const apply = () => {
      el.scrollTop = target
      if (Math.abs(el.scrollTop - target) > 4 && tries++ < 60) {
        requestAnimationFrame(apply)
      } else {
        reportScroll.current?.(el.scrollTop)
      }
    }
    requestAnimationFrame(apply)
  }, [restoreScrollTop, assets.length])

  // 列表条件变化（换关键词 / 换筛选）→ 滚回顶部；不重建组件树（保搜索框焦点与内部状态）
  const lastReset = useRef(resetSignal)
  useEffect(() => {
    if (resetSignal === lastReset.current) return
    lastReset.current = resetSignal
    const el = scrollerEl.current
    if (el) el.scrollTop = 0
    reportScroll.current?.(0)
  }, [resetSignal])

  const chipList = chips ?? []
  // E5：待导入占位卡排在真实资产之前（导入中的图先「现身」，缩略图就绪即在其库内位置点亮）
  const gridData = useMemo<GridItem[]>(
    () => [
      ...(pendingNames ?? []).map((name) => ({ kind: 'pending', name }) as GridItem),
      ...assets.map((asset) => ({ kind: 'asset', asset }) as GridItem),
    ],
    [assets, pendingNames],
  )
  return (
    <div
      className="flex min-h-0 min-w-0 flex-1 flex-col"
      data-testid="asset-grid-root"
      data-thumb={thumbPx}
      style={{ '--thumb-min': `${thumbPx}px` } as React.CSSProperties}
    >
      {/* 工具条 */}
      <div className="flex min-h-11 shrink-0 flex-wrap items-center gap-x-3 gap-y-1 border-b border-line bg-panel px-4 py-1 text-sm text-ink-dim">
        <span className="text-ink">{t('{n} 张', { n: total })}</span>
        <span className="text-ink-dim/60">{t('已加载 {n}', { n: assets.length })}</span>
        {/* KI-16 #5：选择集计数由共用 SelectionBar 统一出（避免两处「已选 N」与重复 testid） */}
        <span className="h-4 w-px bg-line" />
        {searchSlot}
        <span className="ml-auto flex items-center gap-2" data-testid="grid-toolbar">
          {toolbar}
        </span>
      </div>
      {/* P1-2 筛选横幅：筛选生效时列表顶部醒目提示 + 命中数 + 一键清除
          （旧版只有小芯片，且筛选是持久化的 → 重启后空列表像「图丢了」） */}
      <FilterBanner
        chips={chipList}
        total={total}
        onRemoveChip={onRemoveChip}
        onClear={onClearFilter}
      />
      {pendingState ? (
        pendingState
      ) : assets.length === 0 && emptyState ? (
        emptyState
      ) : (
      /* E1 虚拟化网格：只渲染视口附近 DOM，触底自动加载下一页 */
      <VirtuosoGrid
        data={gridData}
        computeItemKey={itemKey}
        context={{ loading, hasMore, loaded: assets.length, total, onLoadMore }}
        endReached={endReached}
        increaseViewportBy={{ top: 600, bottom: 600 }}
        scrollerRef={scrollerRef}
        components={gridComponents}
        itemContent={(_, it) =>
          it.kind === 'pending' ? (
            <PendingCard name={it.name} />
          ) : (
            <AssetCard
              asset={it.asset}
              isSel={selected.has(it.asset.assetId)}
              isActive={activeId === it.asset.assetId}
              selectedIds={selectedIds}
              onPick={onPick}
              onOpen={onOpen}
              onDragAssets={onDragAssets}
              onDragEnd={onDragEnd}
              onContextMenu={onAssetContextMenu}
            />
          )
        }
      />
      )}
    </div>
  )
}

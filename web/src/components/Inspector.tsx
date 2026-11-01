import { useCallback, useEffect, useRef, useState } from 'react'
import { api, formatSize, type AssetView, type TagRecord, type TagResultRecord } from '../lib/api'
import { can, canMoveTo, reasonFor, useCapabilities } from '../lib/caps'
import { useI18n } from '../i18n'
import { groupByTagPrefix, parseTagResultValues } from '../lib/tagFormat'
import { normalizeTagQuery } from '../lib/searchQuery'
import { TagPicker } from './TagPicker'
import { Button } from './Button'
import { PathLink } from './PathLink'
import { SimilarSplitButton } from './SimilarSplitButton'
import { useToast } from '../lib/toast'
import type { SimilarMode } from '../lib/similarMode'

/** E4 检查器动作（页面实现并统一弹 toast / 刷新读模型；KI-11：打分入口已移除） */
export interface InspectorActions {
  reject(rejected: boolean): void
  addTags(names: string[]): void
  /** 解绑标签（Gallery 内部走「乐观移除 + 撤销窗口」，这里只负责落库） */
  removeTag(value: string): void
  setColor(color: string | null): void
  moveTo(dir: string): void
  /** KI-14：可带档位覆盖（split button 菜单选档 = 以新档立即执行） */
  findSimilar(mode?: SimilarMode): void
}

export const COLOR_DEFS: { key: string; label: string; cls: string }[] = [
  { key: 'red', label: '红', cls: 'bg-red-500' },
  { key: 'orange', label: '橙', cls: 'bg-orange-400' },
  { key: 'yellow', label: '黄', cls: 'bg-yellow-400' },
  { key: 'green', label: '绿', cls: 'bg-green-500' },
  { key: 'blue', label: '蓝', cls: 'bg-blue-500' },
]

/** 右栏：检查器。单选 = 内联编辑（标签/色标/找相似）；多选 = 批量编辑（作用于全部选中）。 */
export function Inspector({
  asset,
  batch,
  dirs,
  busy,
  similarBusy,
  similarScore,
  similarMin,
  similarMode,
  onSetSimilarMode,
  tagsVersion,
  actions,
  onPreview,
  variant = 'panel',
}: {
  asset: AssetView | null
  /** 当前选中的资产（视图顺序）；>1 时进入批量编辑模式 */
  batch: AssetView[]
  dirs: { dir: string; count: number }[]
  /** 批量操作请求进行中（禁用按钮防连点） */
  busy: boolean
  similarBusy: boolean
  /** W3：当前图作为「找相似」命中时的相似度（0~1）；非临时集合模式为 undefined */
  similarScore?: number
  /** W3：本次以图搜图生效的下限（null = 未标定模型，后端不过滤） */
  similarMin?: number | null
  /** KI-14：找相似档位（来自页面的 useSimilarMode；split button 主键/菜单共用） */
  similarMode?: SimilarMode
  /** KI-14：切档回调（写偏好 + 触发以新档执行） */
  onSetSimilarMode?: (m: SimilarMode) => void
  /** 标签编辑后递增，触发单选标签区重取 */
  tagsVersion: number
  actions: InspectorActions
  onPreview: (a: AssetView) => void
  /**
   * panel = 常驻右栏（三栏布局遗留形态，仍供单测/其它页面直接用）
   * docked = 右侧非模态抽屉内容（宽度/边框/标题栏/滚动容器都交给 `<DockedPanel>`，
   *          这里只出内容，避免双层标题栏与二级滚动）
   */
  variant?: 'panel' | 'docked'
}) {
  const { t } = useI18n()
  const docked = variant === 'docked'
  const shell = docked
    ? 'flex h-full w-full flex-col'
    : 'flex h-full w-72 shrink-0 flex-col border-l border-line bg-panel xl:w-[320px]'

  const isBatch = batch.length > 1
  const [tags, setTags] = useState<TagRecord[]>([])
  const [tagsErr, setTagsErr] = useState<string | null>(null)
  // UI-LAYOUT（卡 t_11662290 / §9.6）：未选中时检查器收起为 48px 竖条，把宽度还给网格；
  // 用户点竖条可临时展开看说明，选中任意图片后自动恢复全宽（无需再点一次）。
  const [setAside, setSetAside] = useState(false)
  // OCR-E：单选时拉取识别文本（空串 = 未识别，不显示区块）
  const [ocrText, setOcrText] = useState<string | null>(null)
  // 统一标签裁决：单选时拉取打标任务结果（维度名: 模型输出原文/数组快照），无结果不显示区块
  const [tagResults, setTagResults] = useState<TagResultRecord[]>([])

  useEffect(() => {
    setOcrText(null)
    if (!asset || isBatch) return
    let stop = false
    api
      .ocrAssetText(asset.assetId)
      .then((r) => {
        if (!stop && r.ocrText.trim()) setOcrText(r.ocrText)
      })
      .catch(() => {
        /* 旧后端无此接口 / 资产不存在：静默不显示 */
      })
    return () => {
      stop = true
    }
  }, [asset, isBatch])

  useEffect(() => {
    setTags([])
    setTagsErr(null)
    if (!asset || isBatch) return
    let stop = false
    api
      .assetTags(asset.assetId)
      .then((t) => {
        if (!stop) setTags(t)
      })
      .catch((e) => {
        if (!stop) setTagsErr(e instanceof Error ? e.message : String(e))
      })
    return () => {
      stop = true
    }
  }, [asset, tagsVersion, isBatch])

  // 统一标签裁决：打标结果（tag_result：维度名 + 模型输出原文/数组快照）。
  // Promise 链起步：旧后端无此接口 / api 未打桩时在链内拒绝 → 静默不显示，不炸首屏。
  useEffect(() => {
    setTagResults([])
    if (!asset || isBatch) return
    let stop = false
    Promise.resolve()
      .then(() => api.assetTagResults(asset.assetId))
      .then((rs) => {
        // 兼容口径：接口固定返回数组；异常形态（旧桩/代理 mock 返回非数组）一律按空处理，不炸渲染
        if (!stop) setTagResults(Array.isArray(rs) ? rs : [])
      })
      .catch(() => {
        /* 旧后端无此接口 / 资产不存在：静默不显示 */
      })
    return () => {
      stop = true
    }
  }, [asset, tagsVersion, isBatch])

  // ---- UI-UNDO：解绑标签 = 可逆操作 → 不弹确认，乐观移除 + 10s 撤销窗口 ----
  const { toast } = useToast()
  /** 撤销窗口内用户可能已切图：回滚只在「还是同一张」时生效 */
  const assetIdRef = useRef<number | null>(null)
  assetIdRef.current = asset?.assetId ?? null
  const removeTagUndoable = useCallback(
    (value: string) => {
      const assetId = asset?.assetId
      if (!assetId) return
      const snapshot = tags
      // 乐观移除：标签当场消失（不弹确认）
      setTags((prev) => prev.filter((t) => t.value !== value))
      toast.undo({
        message: t('已解绑标签「{value}」', { value }),
        detail: t('10 秒内可撤销；超时才写入图库'),
        onUndo: () => {
          if (assetIdRef.current === assetId) setTags(snapshot)
        },
        // 窗口期结束才真正发请求（Gallery 只做网络 + 重取，不重复弹 toast）
        onCommit: () => {
          actions.removeTag(value)
        },
      })
    },
    [asset, tags, toast, actions],
  )

  if (!asset) {
    // docked：抽屉只在有选中时存在 —— 不留 48px 竖条，也不渲染「未选中」空面板
    if (docked) return null
    // 未选中：默认收起成 48px 竖条（§9.6「折叠态检查器 ≤48px」），点一下临时展开
    if (!setAside) {
      return (
        <div
          data-testid="inspector-strip"
          data-collapsed="1"
          className="flex h-full w-12 shrink-0 flex-col border-l border-line bg-panel"
        >
          <Button
            variant="icon"
            size="none"
            data-testid="inspector-expand"
            aria-label={t('展开检查器（当前未选中图片）')}
            title={t('展开检查器')}
            onClick={() => setSetAside(true)}
            className="h-full w-full flex-col gap-2 rounded-none border-0 bg-panel-2 py-3 text-ink hover:bg-line"
          >
            <span className="text-sm">‹</span>
            <span className="text-[11px] tracking-wider [writing-mode:vertical-rl]">{t('检查器')}</span>
          </Button>
        </div>
      )
    }
    return (
      <div
        className="flex h-full w-72 shrink-0 flex-col border-l border-line bg-panel xl:w-[320px]"
        data-testid="inspector-empty"
        data-collapsed="0"
      >
        <div className="flex items-center justify-between border-b border-line px-4 py-3">
          <span className="text-xs font-semibold tracking-wider text-ink-dim">{t('检查器')}</span>
          <Button
            variant="icon"
            data-testid="inspector-collapse"
            aria-label={t('收起检查器')}
            title={t('收起检查器（把宽度还给网格）')}
            onClick={() => setSetAside(false)}
          >
            ›
          </Button>
        </div>
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center">
          <span className="text-2xl text-ink-dim"> ☗ </span>
          <p className="text-sm text-ink-dim">{t('未选中图片')}</p>
          <p className="text-xs leading-5 text-ink-dim/80">{t('点击中部网格中的任意图片，此处显示标签、元数据与可用操作')}</p>
        </div>
      </div>
    )
  }

  if (isBatch) {
    return (
      <BatchPanel batch={batch} dirs={dirs} busy={busy} actions={actions} variant={variant} />
    )
  }

  const rejected = asset.approvalStatus === 'rejected'
  const meta: [string, string][] = [
    [t('尺寸'), asset.width && asset.height ? `${asset.width} × ${asset.height}` : '—'],
    [t('大小'), formatSize(asset.fileSz)],
    [t('格式'), asset.fileExt.toUpperCase()],
    [t('导入时间'), new Date(asset.importedAt * 1000).toLocaleString('zh-CN', { hour12: false })],
    [t('存放'), asset.storeMode === 'copyin' ? t('复制入库') : t('原地索引')],
    [t('状态'), rejected ? t('已拒选') : asset.missing === 1 ? t('文件失联') : t('正常')],
    ['ID', String(asset.assetId)],
  ]
  return (
    <div className={shell}>
      {!docked && (
        <div className="border-b border-line px-4 py-3 text-xs font-semibold tracking-wider text-ink-dim">{t('检查器')}</div>
      )}
      <div className={docked ? 'min-h-0' : 'min-h-0 flex-1 overflow-y-auto'}>
        <button
          type="button"
          className="aspect-4/3 w-full overflow-hidden bg-panel-2"
          onClick={() => onPreview(asset)}
          title={t('点击预览大图')}
        >
          <img
            src={api.thumbUrl(asset.assetId, 256)}
            alt={asset.fileName}
            loading="lazy"
            decoding="async"
            className="h-full w-full object-cover"
          />
        </button>
        <div className="p-4">
          <p className="truncate text-sm font-semibold" title={asset.fileName}>{asset.fileName}</p>
          {/* KI-10：文件路径行 = 唯一可点入口（在文件夹中显示 / 复制路径，见 <PathLink>） */}
          <PathLink path={asset.absPath} mode="reveal" className="mt-0.5" />

          <dl className="mt-4 space-y-1.5 text-xs">
            {meta.map(([k, v]) => (
              <div key={k} className="flex justify-between gap-2">
                <dt className="shrink-0 text-ink-dim">{k}</dt>
                <dd className="truncate text-right" title={v}>{v}</dd>
              </div>
            ))}
          </dl>

          {/* OCR-E：识别文本（只读；空 = 未跑识别） */}
          {ocrText && (
            <div className="mt-4" data-testid="insp-ocr">
              <p className="text-xs font-semibold text-ink-dim">{t('识别文字')}</p>
              <pre className="mt-1.5 max-h-40 overflow-y-auto whitespace-pre-wrap break-all rounded border border-line bg-panel-2 p-2 text-xs leading-5">
                {ocrText}
              </pre>
            </div>
          )}
          {/* EXIF2：拍摄元数据（无 EXIF 如实显示「无元数据」，不显示空白误导） */}
          <ExifPanel asset={asset} />

          {/* 色标 */}
          <div className="mt-4">
            <p className="text-xs font-semibold text-ink-dim">{t('色标')}</p>
            <div className="mt-1.5 flex items-center gap-1.5" data-testid="insp-colors">
              {COLOR_DEFS.map((c) => (
                <Button
                  key={c.key}
                  variant="icon"
                  disabled={busy}
                  active={asset.colorLabel === c.key}
                  data-testid={`insp-color-${c.key}`}
                  onClick={() => actions.setColor(c.key)}
                  title={t(c.label)}
                  className="rounded-full"
                >
                  <span className={`h-4 w-4 rounded-full ${c.cls}`} />
                </Button>
              ))}
              {asset.colorLabel && (
                <Button disabled={busy} className="ml-1" onClick={() => actions.setColor(null)}>
                  {t('清除')}
                </Button>
              )}
            </div>
          </div>

          {/* 标签（内联增删；key-val 前缀分组展示，见 <TagGroupChips>） */}
          <TagEditor tags={tags} tagsErr={tagsErr} busy={busy} onAdd={actions.addTags} onRemove={removeTagUndoable} />

          {/* 统一标签裁决：打标结果（维度名: 值；数组快照逐项 chip；无结果不显示） */}
          <TagResultsPanel results={tagResults} />

          {/* 移动到目录 */}
          <MovePicker dirs={dirs} busy={busy} onMove={actions.moveTo} />

          <div className="mt-5 space-y-1.5">
            <Button size="md" className="w-full" onClick={() => onPreview(asset)}>
              {t('预览大图')}
            </Button>
            {/* DECouple-FE：外链「打开原图」被 Tauri 丢弃（target=_blank 点击无反应，KI41 同款根因）
                —— 应用内看原图唯一入口 = 上面的「预览大图」（一个能力一个入口）。 */}
            {/* KI-14：找相似档位与执行都住在这里（split button）——工具条不再放模式开关 */}
            <SimilarSplitButton
              mode={similarMode ?? 'semantic'}
              onModeChange={onSetSimilarMode ?? (() => {})}
              onRun={(m) => actions.findSimilar(m)}
              busy={similarBusy}
            />
            {similarScore !== undefined && (
              <SimilarScoreLine score={similarScore} min={similarMin} />
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

/**
 * W3：「找相似」命中行的相似度（DOM 存证用独立组件，便于单测）。
 * min = 本次生效下限（null/0 = 未标定模型，后端没过滤 → 不显示分母）。
 */
export function SimilarScoreLine({ score, min }: { score: number; min?: number | null }) {
  const { t } = useI18n()
  const pct = Math.round(score * 100)
  return (
    <div
      data-testid="insp-similar-score"
      className="text-center text-[11px] text-ink-dim"
      title={t('与本次查询图的余弦相似度（下限按嵌入模型标定）')}
    >
      {t('相似度 {pct}%', { pct })}
      {typeof min === 'number' && min > 0 ? t('（本次下限 {pct}%）', { pct: Math.round(min * 100) }) : ''}
    </div>
  )
}

/** E4 批量编辑面板：拒选/打标/移动作用于全部选中 */
function BatchPanel({
  batch,
  dirs,
  busy,
  actions,
  variant = 'panel',
}: {
  batch: AssetView[]
  dirs: { dir: string; count: number }[]
  busy: boolean
  actions: InspectorActions
  variant?: 'panel' | 'docked'
}) {
  const { t } = useI18n()
  const docked = variant === 'docked'
  const allRejected = batch.every((a) => a.approvalStatus === 'rejected')
  const strip = batch.slice(0, 8)
  const rest = batch.length - strip.length
  return (
    <div
      className={
        docked
          ? 'flex h-full w-full flex-col'
          : 'flex h-full w-72 shrink-0 flex-col border-l border-line bg-panel xl:w-[320px]'
      }
    >
      {!docked && (
        <div className="flex items-center justify-between border-b border-line px-4 py-3">
          <span className="text-xs font-semibold tracking-wider text-ink-dim">{t('批量编辑')}</span>
          <span className="rounded border border-accent/60 px-1.5 py-0.5 text-xs text-accent" data-testid="batch-count">
            {t('{n} 张', { n: batch.length })}
          </span>
        </div>
      )}
      {/* docked：标题与张数已由 <DockedPanel> 的 header（title/subtitle）给出，这里不再重复 */}
      <div className={docked ? 'min-h-0 p-4' : 'min-h-0 flex-1 overflow-y-auto p-4'}>
        {/* 缩略图条（前 8 张 + 计数） */}
        <div className="flex flex-wrap gap-1.5">
          {strip.map((a) => (
            <img
              key={a.assetId}
              src={api.thumbUrl(a.assetId, 256)}
              alt={a.fileName}
              loading="lazy"
              className="h-12 w-16 rounded border border-line object-cover"
            />
          ))}
          {rest > 0 && (
            <span className="flex h-12 w-12 items-center justify-center rounded border border-line text-xs text-ink-dim">
              +{rest}
            </span>
          )}
        </div>

        <p className="mt-4 text-xs leading-5 text-ink-dim">
          {t('以下操作作用于全部 {n} 张选中图片；逐条回执，部分失败会在 toast 中逐条列出。', { n: batch.length })}
        </p>

        {/* 拒选 */}
        <Button
          size="md"
          tone={allRejected ? 'default' : 'danger'}
          disabled={busy}
          data-testid="batch-reject"
          className="mt-4 w-full"
          onClick={() => actions.reject(!allRejected)}
        >
          {allRejected ? t('取消拒选（全部）') : t('拒选（全部 {n} 张）', { n: batch.length })}
        </Button>

        {/* 批量打标 */}
        <TagEditor tags={[]} tagsErr={null} busy={busy} onAdd={actions.addTags} onRemove={undefined} batchHint />

        {/* 批量移动 */}
        <MovePicker dirs={dirs} busy={busy} onMove={actions.moveTo} />
      </div>
    </div>
  )
}

/** 标签增删（单选 = 列表可删；批量 = 只加）。
 * 展示层按第一个 `-` 前缀分组（key = 任务名）：组标题一行 + 组内 chip 只显示前缀后的值；
 * 裸值照旧平铺。筛选/删除仍用完整 value（testid `tag-remove-<完整 value>`）。 */
function TagEditor({
  tags,
  tagsErr,
  busy,
  onAdd,
  onRemove,
  batchHint,
}: {
  tags: TagRecord[]
  tagsErr: string | null
  busy: boolean
  onAdd: (values: string[]) => void
  onRemove?: (value: string) => void
  batchHint?: boolean
}) {
  const { t } = useI18n()
  const [input, setInput] = useState('')
  const submit = () => {
    const values = input
      .split(/[,，]/)
      .map((s) => s.trim())
      .filter(Boolean)
    if (values.length === 0) return
    onAdd(values)
    setInput('')
  }
  const groups = groupByTagPrefix(tags, (t) => t.value)
  return (
    <div className="mt-4">
      <p className="text-xs font-semibold text-ink-dim">{t('标签')}{batchHint ? t('（加到全部选中）') : ''}</p>
      {tagsErr && <span className="text-xs text-red-400">{t('加载失败：{err}', { err: tagsErr })}</span>}
      {!tagsErr && !batchHint && tags.length === 0 && (
        <span className="text-xs text-ink-dim">{t('暂无标签')}</span>
      )}
      {groups.map((g) => (
        <div key={g.key || '_plain'} className="mt-1.5" data-testid={`tag-group-${g.key || 'plain'}`}>
          {g.key && (
            <p
              className="mb-1 text-[11px] font-medium tracking-wide text-ink-dim"
              title={t('维度「{key}」（任务名做前缀）', { key: g.key })}
            >
              {g.key}
            </p>
          )}
          <div className="flex flex-wrap gap-1.5">
            {g.entries.map(({ value, display }) => (
              <span
                key={value}
                className="flex items-center gap-1 rounded-full border border-line bg-panel-2 px-2 py-0.5 text-xs"
              >
                {display}
                {onRemove && (
                  <Button
                    variant="icon"
                    size="none"
                    disabled={busy}
                    data-testid={`tag-remove-${value}`}
                    onClick={() => onRemove(value)}
                    title={t('移除标签「{value}」', { value })}
                    className="h-6 w-6 shrink-0 rounded-full border-0 bg-transparent text-ink hover:bg-line hover:text-red-400"
                  >
                    ×
                  </Button>
                )}
              </span>
            ))}
          </div>
        </div>
      ))}
      <div className="mt-2 flex gap-1.5">
        <TagPicker
          testid="tag-input"
          value={input}
          onChange={setInput}
          // 选中候选 = 直接加这个标签（完整 value 落库），清空输入
          onPick={(v) => {
            onAdd([v])
            setInput('')
          }}
          onCommit={(raw) => {
            const values = raw
              .split(/[,，]/)
              .map((s) => normalizeTagQuery(s))
              .filter(Boolean)
            if (values.length === 0) return
            onAdd(values)
            setInput('')
          }}
          scenario="tag"
          disabled={busy}
          placeholder={t('输入标签值，回车新建；逗号分隔可多个')}
          className="min-w-0 flex-1 rounded border border-line bg-panel-2 px-2 py-1 text-xs outline-none placeholder:text-ink-dim/50 focus:border-accent"
          panelClassName="absolute top-full left-0 z-40 mt-1 max-h-56 w-64 overflow-y-auto rounded-md border border-line bg-panel p-1 text-xs shadow-lg"
        />
        <Button disabled={busy || !input.trim()} data-testid="tag-add" className="shrink-0" onClick={submit}>
          {t('添加')}
        </Button>
      </div>
    </div>
  )
}

/** 统一标签裁决：打标结果区（tag_result 读模型）。维度名一行，值为数组快照 → 逐项 chip；
 * 非数组原文照抄；空数组显示「（空）」。无结果 → 整块不渲染。 */
export function TagResultsPanel({ results }: { results: TagResultRecord[] }) {
  const { t } = useI18n()
  const list = Array.isArray(results) ? results : []
  if (list.length === 0) return null
  return (
    <div className="mt-4" data-testid="insp-tag-results">
      <p className="text-xs font-semibold text-ink-dim">{t('打标结果')}</p>
      <dl className="mt-1.5 space-y-1.5 text-xs">
        {list.map((r) => {
          const values = parseTagResultValues(r.value)
          return (
            <div key={r.id} data-testid="insp-tag-result-row" className="flex justify-between gap-2">
              <dt className="shrink-0 text-ink-dim" title={r.name}>
                {r.name}
              </dt>
              <dd className="flex min-w-0 flex-wrap justify-end gap-1 text-right">
                {values === null ? (
                  <span className="break-all" title={r.value}>
                    {r.value}
                  </span>
                ) : values.length === 0 ? (
                  <span className="text-ink-dim">{t('（空）')}</span>
                ) : (
                  values.map((v) => (
                    <span
                      key={v}
                      className="rounded-full border border-line bg-panel-2 px-2 py-0.5 text-xs"
                      data-testid="insp-tag-result-value"
                    >
                      {v}
                    </span>
                  ))
                )}
              </dd>
            </div>
          )
        })}
      </dl>
    </div>
  )
}

/** 目录下拉 + 移动按钮（单选/批量共用）。
 *
 * WEBCAP：形态能力开关（`/api/capabilities`）驱动 —— 网页容器形态的「本地磁盘」是服务器
 * 文件系统（库根常只读/跨设备），该形态不对用户承诺「移动到目录」：下拉与按钮 disabled，
 * 并给出人话 reason（指向桌面版 / copy-in），不是静默失败。桌面形态里也要逐目录判
 * 「是否可写库根」，只读根（如外接只读卷）的选项一并灰掉。 */
function MovePicker({
  dirs,
  busy,
  onMove,
}: {
  dirs: { dir: string; count: number }[]
  busy: boolean
  onMove: (dir: string) => void
}) {
  const { t } = useI18n()
  const caps = useCapabilities()
  const featureOn = can(caps, 'moveAsset')
  const reason = featureOn ? '' : reasonFor(caps, 'moveAsset')
  const [dir, setDir] = useState('')
  const disabled = busy || dirs.length === 0 || !featureOn
  return (
    <div className="mt-4">
      <p className="text-xs font-semibold text-ink-dim">{t('移动到目录')}</p>
      <div className="mt-1.5 flex gap-1.5">
        <select
          value={dir}
          disabled={disabled}
          data-testid="move-select"
          title={reason || undefined}
          aria-describedby={reason ? 'move-disabled-reason' : undefined}
          onChange={(e) => setDir(e.target.value)}
          className="min-w-0 flex-1 truncate rounded border border-line bg-panel-2 px-2 py-1 text-xs outline-none focus:border-accent disabled:opacity-40"
        >
          <option value="">{dirs.length === 0 ? t('（暂无目录）') : t('选择目录…')}</option>
          {dirs.map((d) => {
            const ok = canMoveTo(caps, d.dir)
            return (
              <option key={d.dir} value={d.dir} disabled={!ok}>
                {t('{dir}（{count}）', { dir: d.dir, count: d.count })}{ok ? '' : t(' · 只读，不可移')}
              </option>
            )
          })}
        </select>
        <Button
          disabled={disabled || !dir || !canMoveTo(caps, dir)}
          data-testid="move-apply"
          title={reason || undefined}
          onClick={() => onMove(dir)}
          className="shrink-0"
        >
          {t('移动')}
        </Button>
      </div>
      {!featureOn && reason && (
        <p id="move-disabled-reason" data-testid="move-disabled-reason" className="mt-1.5 text-xs leading-5 text-ink-dim">
          {reason}
        </p>
      )}
    </div>
  )
}


/** EXIF2：拍摄元数据面板。exif 解析过但无元数据 → 明确「无元数据」；未解析（null）→ 提示可回填。 */
export function ExifPanel({ asset }: { asset: AssetView }) {
  const { t } = useI18n()
  const parsed = asset.exif != null
  // ⚠️ 画幅（aspectClass）**不算 EXIF 元数据**：它是像素宽高派生的，任何图都有。
  // 若把它算进 hasAny，无 EXIF 的图会渲染成「8 行全 —」的假表格（看着像有元数据），
  // 违背卡面「无 EXIF 要明说无元数据、不误导」的要求 —— 2026-10-16 UI 走查实测到（EXIF2）。
  const hasAny =
    asset.takenAt != null ||
    asset.cameraModel != null ||
    asset.lensModel != null ||
    asset.iso != null
  const aspectLabel =
    asset.aspectClass === 'landscape'
      ? t('横图（{ratio}）', { ratio: asset.aspectRatio ?? '' })
      : asset.aspectClass === 'portrait'
        ? t('竖图（{ratio}）', { ratio: asset.aspectRatio ?? '' })
        : asset.aspectClass === 'square'
          ? t('方图（{ratio}）', { ratio: asset.aspectRatio ?? '' })
          : null
  const rows: [string, string][] = parsed
    ? [
        [t('拍摄时间'), asset.takenAt != null ? new Date(asset.takenAt * 1000).toLocaleString('zh-CN', { hour12: false }) : '—'],
        [t('相机'), asset.cameraModel ? [asset.cameraMake, asset.cameraModel].filter(Boolean).join(' ') : '—'],
        [t('镜头'), asset.lensModel ?? '—'],
        ['ISO', asset.iso != null ? String(asset.iso) : '—'],
        [t('焦距'), asset.focalLenMm != null ? `${asset.focalLenMm}mm` : '—'],
        [t('光圈'), asset.aperture != null ? `f/${asset.aperture}` : '—'],
        [t('快门'), asset.shutterText ?? '—'],
        [t('画幅'), aspectLabel ?? '—'],
      ]
    : []
  return (
    <div className="mt-4" data-testid="insp-exif">
      <p className="text-xs font-semibold text-ink-dim">{t('拍摄元数据')}</p>
      {!parsed ? (
        <p className="mt-1.5 text-xs leading-5 text-ink-dim" data-testid="insp-exif-unparsed">
          {t('元数据未解析 — 可在设置里运行「存量回填」')}
        </p>
      ) : !hasAny ? (
        <div data-testid="insp-exif-none">
          <p className="mt-1.5 text-xs text-ink-dim">{t('无元数据')}</p>
          {aspectLabel && (
            <p className="mt-1 text-xs text-ink-dim/80">{t('画幅 {aspect}', { aspect: aspectLabel })}</p>
          )}
        </div>
      ) : (
        <dl className="mt-1.5 space-y-1.5 text-xs" data-testid="insp-exif-rows">
          {rows.map(([k, v]) => (
            <div key={k} className="flex justify-between gap-2">
              <dt className="shrink-0 text-ink-dim">{k}</dt>
              <dd className="break-all text-right" title={v}>{v}</dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  )
}

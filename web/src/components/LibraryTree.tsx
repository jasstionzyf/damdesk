import { useMemo, useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { tGlobal, useI18n } from '../i18n'
import type { DirEntry, LibraryStats, TagCount } from '../lib/api'
import { TagPicker } from './TagPicker'
import { can, canMoveTo, reasonFor, useCapabilities } from '../lib/caps'
import { groupByTagPrefix } from '../lib/tagFormat'
import { takeDragPayload } from '../lib/dnd'
import { PathLink } from './PathLink'

/**
 * E1 过滤模型：多维度可叠加（目录 × 标签 × 未打标），
 * view=recent 是独占视图。侧栏选中态与网格工具条筛选芯片同源双向联动。
 */
export interface LibraryFilter {
  view: 'all' | 'recent'
  untagged?: boolean
  dir?: string
  tag?: string
  /** DESKTOP-MISSING-STAT-1：仅看失联（missing=1） */
  missing?: number
  /** D3：仅看无法预览（解码失败，resource 无尺寸）= 1 */
  broken?: number
  // ---- EXIF2 元数据维度（task） ----
  /** 画幅过滤 */
  aspect?: 'landscape' | 'portrait' | 'square'
  /** 相机型号精确匹配 */
  cameraModel?: string
  /** 拍摄日期范围（unix 秒，含端点） */
  takenFrom?: number
  takenTo?: number
  /** 排序键（视图选项，不产芯片） */
  sort?: 'importedAt' | 'takenAt'
}

export const defaultFilter: LibraryFilter = { view: 'all' }

export function filterKey(f: LibraryFilter): string {
  return JSON.stringify(f)
}

/** 筛选芯片（当前条件可视化，field 供一键移除） */
export interface FilterChip {
  field:
    | 'dir'
    | 'tag'
    | 'untagged'
    | 'missing'
    | 'broken'
    | 'aspect'
    | 'cameraModel'
    | 'takenRange'
    /** KI-16 #7：来源智能文件夹（`?sf=<id>`）；移除 = 解除绑定（条件芯片各自保留） */
    | 'sf'
    /** 快速筛选关键词（不在 LibraryFilter 里，由图库页的 query 派生；移除=清空搜索框） */
    | 'query'
  label: string
}

/** 由过滤条件派生芯片列表（顺序即展示顺序）。模块级纯函数 → 用 tGlobal（读当前 locale）。 */
export function chipsOf(f: LibraryFilter): FilterChip[] {
  const chips: FilterChip[] = []
  if (f.dir) {
    const segs = pathSegs(f.dir)
    chips.push({ field: 'dir', label: tGlobal('目录 {dir}', { dir: segs[segs.length - 1] ?? f.dir }) })
  }
  if (f.tag) chips.push({ field: 'tag', label: tGlobal('标签 {tag}', { tag: f.tag }) })
  if (f.untagged) chips.push({ field: 'untagged', label: tGlobal('未打标') })
  if (f.missing) chips.push({ field: 'missing', label: tGlobal('仅失联') })
  if (f.broken) chips.push({ field: 'broken', label: tGlobal('仅无法预览') })
  // EXIF2 芯片
  if (f.aspect) {
    const label =
      f.aspect === 'landscape' ? tGlobal('横图') : f.aspect === 'portrait' ? tGlobal('竖图') : tGlobal('方图')
    chips.push({ field: 'aspect', label })
  }
  if (f.cameraModel) chips.push({ field: 'cameraModel', label: tGlobal('相机 {model}', { model: f.cameraModel }) })
  if (f.takenFrom != null || f.takenTo != null) {
    const fmt = (t?: number) => (t != null ? new Date(t * 1000).toLocaleDateString('zh-CN') : '…')
    chips.push({ field: 'takenRange', label: tGlobal('拍摄 {from} ~ {to}', { from: fmt(f.takenFrom), to: fmt(f.takenTo) }) })
  }
  return chips
}

/** 目录树节点（absPath 为 key，count 聚合子目录） */
export interface DirNode {
  dir: string
  rel: string
  count: number
  depth: number
}

/** 去尾分隔符（批次根 `D:\pics\` 与 `D:\pics` 同形；`/` 根保留） */
export function trimSep(p: string): string {
  const t = p.length > 1 ? p.replace(/[\\/]+$/, '') : p
  return t || p
}

/** 路径分隔符无关：`dir` 是否等于 `root` 或在其下（Windows `D:\pics` 与 POSIX `/pics` 都成立） */
export function isUnderRoot(dir: string, root: string): boolean {
  const r = trimSep(root)
  // 文件系统根（`/`、`D:`、`\`）本身没有尾分隔符可加
  if (r === '/' || r === '\\' || /^[A-Za-z]:$/.test(r)) return dir.startsWith(r)
  return dir === r || dir.startsWith(r + '/') || dir.startsWith(r + '\\')
}

/** 路径按 `/` 或 `\` 切段（Windows 盘符路径不因写死 `'/'` 变成单段） */
export function pathSegs(p: string): string[] {
  return p.split(/[\\/]/).filter(Boolean)
}

/** 由扁平 distinct 目录构造层级（相对最近批次根），父节点 count 聚合全部后代 */
export function buildTree(dirs: DirEntry[], roots: string[]): DirNode[] {
  const relOf = (dir: string): { rel: string; depth: number } => {
    const root = roots
      .map(trimSep)
      .filter((r) => isUnderRoot(dir, r))
      .sort((a, b) => b.length - a.length)[0]
    const rel = root ? dir.slice(root.length).replace(/^[\\/]+/, '') : dir
    const depth = pathSegs(rel).length
    return { rel, depth }
  }
  const nodes = new Map<string, DirNode>()
  for (const { dir, count } of dirs) {
    const { rel, depth } = relOf(dir)
    nodes.set(dir, { dir, rel, count, depth })
  }
  // 聚合：祖先目录累计后代 count
  const out = [...nodes.values()].sort((a, b) => a.dir.localeCompare(b.dir))
  for (const n of out) {
    for (const m of out) {
      if (n !== m && isUnderRoot(m.dir, n.dir)) n.count += m.count
    }
  }
  return out
}

function Row({
  icon,
  label,
  title,
  count,
  active,
  indent,
  onClick,
  dropHint,
  onDropIds,
  testid,
  dropDisabledReason,
}: {
  icon: string
  label: string
  title?: string
  count?: number
  active: boolean
  indent?: boolean
  onClick: () => void
  /** E4：可拖放目标（拖入高亮，drop 上报 ids） */
  dropHint?: boolean
  onDropIds?: (ids: number[]) => void
  testid?: string
  /** WEBCAP：本行不能作拖放目标时的人话原因（非空 → 不可 drop + data-drop-disabled=1） */
  dropDisabledReason?: string
}) {
  const [hover, setHover] = useState(false)
  const droppable = !!onDropIds && !dropDisabledReason
  return (
    <button
      type="button"
      data-testid={testid}
      onClick={onClick}
      onDragOver={(e) => {
        if (!droppable) return
        e.preventDefault()
        e.dataTransfer.dropEffect = 'move'
        if (!hover) setHover(true)
      }}
      onDragLeave={() => {
        if (hover) setHover(false)
      }}
      onDrop={(e) => {
        e.preventDefault()
        setHover(false)
        if (droppable) onDropIds(takeDragPayload())
      }}
      data-drop-disabled={dropDisabledReason ? '1' : undefined}
      title={dropDisabledReason ? `${title ?? label}（${dropDisabledReason}）` : title}
      aria-disabled={dropDisabledReason ? true : undefined}
      className={`flex w-full items-center gap-2 rounded-md py-1.5 pr-2 text-left text-sm transition-colors ${
        indent ? 'pl-7' : 'pl-2'
      } ${
        hover
          ? 'bg-accent text-white ring-2 ring-accent'
          : active
            ? 'bg-accent-soft text-ink'
            : 'text-ink-dim hover:bg-panel-2 hover:text-ink'
      } ${droppable && dropHint && !hover ? 'outline-1 outline-dashed outline-line/70' : ''}`}
    >
      <span className="w-4 shrink-0 text-center text-xs">{icon}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {count !== undefined && (
        <span className="shrink-0 text-xs tabular-nums text-ink-dim">{count}</span>
      )}
    </button>
  )
}

/** 左栏：库/智能筛选 + 目录树 + 标签（/api/library/dirs · /api/library/tags 真实数据）。
 *  E1：过滤多维度叠加（目录×标签），选中态与工具条芯片联动。
 *  E4：目录行 = 拖放移动目标，标签行 = 拖放打标目标，拖拽悬停高亮。 */
export function LibraryTree({
  stats,
  dirs,
  roots,
  tags,
  filter,
  dragActive,
  onSelect,
  onRefreshDirs,
  onDropMove,
  onDropTag,
}: {
  stats: LibraryStats | null
  dirs: DirEntry[]
  roots: string[]
  /** E4：侧栏标签云（拖放打标目标 + 点击过滤） */
  tags: TagCount[]
  filter: LibraryFilter
  /** E4：网格正在拖拽（显示放置提示 + 行虚线框） */
  dragActive: boolean
  onSelect: (f: LibraryFilter) => void
  /** DESKTOP-DIRS-TREE-1：目录树空态给「重新扫描库」出口（重拉 dirs/stats/tags） */
  onRefreshDirs: () => void
  onDropMove: (ids: number[], dir: string) => void
  onDropTag: (ids: number[], name: string) => void
}) {
  const { t } = useI18n()
  const navigate = useNavigate()
  const tree = useMemo(() => buildTree(dirs, roots), [dirs, roots])
  /** KI-13（task）：侧栏标签筛选输入的本地草稿（选中候选才落到 filter/URL） */
  const [tagQuery, setTagQuery] = useState('')
  // WEBCAP：目录行 = 移动的拖放目标 → 按形态能力 + 该目录是否可写库根逐行判定
  // （网页形态一律不可拖放；桌面形态里只读卷的目录也不可拖放）
  const caps = useCapabilities()
  const moveOn = can(caps, 'moveAsset')
  const moveReason = moveOn ? '' : reasonFor(caps, 'moveAsset')
  const dirDropReason = (dir: string): string | undefined => {
    if (!moveOn) return moveReason || t('当前形态不支持拖动移动')
    const rootsTxt = caps?.writableRoots.join('、') || t('无可写根')
    return canMoveTo(caps, dir) ? undefined : t('该目录不在可写库根内（{roots}）', { roots: rootsTxt })
  }
  const total = stats?.assets ?? 0
  const untagged = stats ? stats.assets - stats.tagged : 0
  const hasDim = !!(
    filter.dir ||
    filter.tag ||
    filter.untagged ||
    filter.missing ||
    filter.broken
  )
  // G5-A：失联数由「失败即标」维护（后端 core-storage presence），前端只读数字，不显示新鲜度
  const missing = stats?.missing ?? 0

  return (
    <div className="flex h-full w-60 shrink-0 flex-col border-r border-line bg-panel">
      <div className="flex flex-col gap-0.5 p-2">
        <p className="px-2 pt-1 pb-2 text-xs font-semibold tracking-wider text-ink-dim">{t('库')}</p>
        <Row
          icon="▣"
          label={t("全部图片")}
          count={total}
          active={filter.view === 'all' && !hasDim}
          onClick={() => onSelect({ view: 'all' })}
        />
        <Row
          icon="◷"
          label={t("最近导入")}
          count={Math.min(24, total)}
          active={filter.view === 'recent'}
          onClick={() => onSelect({ view: 'recent' })}
        />
        <Row
          icon="◌"
          label={t("未打标")}
          count={untagged}
          active={filter.view === 'all' && filter.untagged === true}
          onClick={() => onSelect({ ...filter, view: 'all', untagged: true, tag: undefined })}
        />
        {/* G5-A（裁决 2026-10-19）：失联只有一个入口 —— 这一行。点击 = `missing=1` 筛选。
            数字由「失败即标」写入（导出原图 / 读原图链接 / 相似图重算遇 NotFound），
            没有核查按钮、没有新鲜度提示：不做成常驻功能模块。 */}
        <Row
          icon="⚠"
          label={t("失联")}
          count={missing}
          title={t('失联 = 导出或读取原图时发现文件已不在磁盘（失败即标）；点这里只看这些资产')}
          active={filter.view === 'all' && filter.missing === 1}
          onClick={() =>
            onSelect({ ...filter, view: 'all', missing: 1, untagged: undefined })
          }
          testid="row-missing"
        />
        {/* D3：无法预览 = 文件在盘上但解不出像素（扩展名对、内容不是图片/已损坏）。
            与「失联」是两回事：失联是文件没了，这里是文件在但打不开。
            计数与 /api/library/assets?broken=1 同一口径（resource 无正尺寸）。 */}
        <Row
          icon="⊘"
          label={t("无法预览")}
          count={stats?.broken ?? 0}
          title={t('文件在盘上但解不出像素（扩展名对、内容不是图片/已损坏）。点这里只看这些资产；修好文件后重新读取该资产会自动恢复')}
          active={filter.view === 'all' && filter.broken === 1}
          onClick={() => onSelect({ ...filter, view: 'all', broken: 1, untagged: undefined })}
          testid="row-broken"
        />
      </div>
      <div className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto p-2">
        <p className="px-2 pt-1 pb-2 text-xs font-semibold tracking-wider text-ink-dim">
          {t('目录')}
          <span className="ml-2 font-normal normal-case">{t('（拖图到此=移动）')}</span>
        </p>
        {/* KI-10：库根与当前目录 = 目录级「打开文件夹」。侧栏只有 240px，路径竖排；
            树里每一行都是筛选/拖放目标（一个 row 一个 button），不能在里面再嵌按钮，
            所以「打开任意目录」走「选中目录 → 当前目录 → 打开文件夹」这一条路。 */}
        {roots.length > 0 && (
          <div className="mb-1 space-y-1 px-2" data-testid="library-roots">
            <p className="text-[11px] text-ink-dim">{t('库根')}</p>
            {roots.map((r) => (
              /* 侧栏只有 240px → compact：名 + 卷/父目录，单行截断（全路径进 title / 复制路径）。
                 同列表库根互为 siblings：basename 重名时自动多露一级父目录。 */
              <PathLink key={r} path={r} mode="open" display="compact" siblings={roots} />
            ))}
          </div>
        )}
        {filter.dir && (
          <div className="mb-2 px-2" data-testid="library-current-dir">
            <p className="text-[11px] text-ink-dim">{t('当前目录')}</p>
            <PathLink path={filter.dir} mode="open" display="compact" siblings={roots} />
          </div>
        )}
        {tree.length === 0 && (
          <div className="flex flex-col gap-1 px-2 py-1">
            <p className="text-xs leading-5 text-ink-dim">{t('当前库没有可归纳的目录')}</p>
            <button
              type="button"
              data-testid="dirs-refresh"
              onClick={onRefreshDirs}
              title={t('重新读取库内资产目录（不重新扫描磁盘）')}
              className="w-fit rounded border border-line px-1.5 py-0.5 text-[11px] text-ink-dim transition-colors hover:border-accent hover:text-ink"
            >
              {t('重新扫描库')}
            </button>
          </div>
        )}
        {tree.map((n) => {
          const segs = pathSegs(n.rel)
          const label = segs[segs.length - 1] || n.dir
          const noDrop = dirDropReason(n.dir)
          return (
            <div key={n.dir} className="group flex items-center gap-0.5">
              <div className="min-w-0 flex-1">
                <Row
                  icon={n.depth === 0 ? '▤' : '·'}
                  label={label}
                  title={n.dir}
                  count={n.count}
                  indent={n.depth > 0}
                  active={!!filter.dir && filter.dir === n.dir}
                  onClick={() => onSelect({ ...filter, view: 'all', dir: n.dir })}
                  dropHint={dragActive && !noDrop}
                  dropDisabledReason={noDrop}
                  onDropIds={(ids) => ids.length > 0 && onDropMove(ids, n.dir)}
                  testid={`drop-dir-${n.dir}`}
                />
              </div>
              {/* KI-7：库内「此文件夹查重」= 把范围带到 /dedup（写操作/回收站都在那一页，
                  不在侧栏重复实现一套）。悬停/聚焦才显形，避免每个目录行都挂一个常驻按钮。 */}
              <button
                type="button"
                data-testid={`dedup-dir-${n.dir}`}
                title={t('此文件夹查重：只在这个目录内部找重复（{dir}）', { dir: n.dir })}
                aria-label={t('此文件夹查重 {dir}', { dir: label })}
                onClick={() => navigate(`/dedup?range=${encodeURIComponent(`dir:${n.dir}`)}`)}
                className="shrink-0 rounded px-1 text-[11px] text-ink-dim opacity-0 transition-opacity hover:text-accent focus:opacity-100 group-hover:opacity-100"
              >
                {t('查重')}
              </button>
            </div>
          )
        })}

        <p className="mt-3 px-2 pt-1 pb-2 text-xs font-semibold tracking-wider text-ink-dim">
          {t('标签')}
          <span className="ml-2 font-normal normal-case">{t('（拖图到此=打标）')}</span>
          {/* task：全量标签总览的唯一入口（低频 → 不进主导航；标签云永远只作摘要） */}
          <Link
            to="/tags"
            data-testid="sidebar-tags-all"
            title={t('查看全部标签：分组浏览 + 合并同义值{cap}', { cap: tags.length >= 30 ? t('（当前仅显示前 30 条）') : '' })}
            className="ml-2 font-normal normal-case text-accent hover:underline"
          >
            {t('全部 →')}
          </Link>
        </p>
        {/* KI-13 / R40（task）：标签筛选输入 = 候选 + 选中即过滤（写 URL 的 tags=，
            与点标签行同一动作）；此前这里是裸 input/无入口，候选逻辑只在搜索页自写一份。 */}
        <div className="px-2 pb-1">
          <TagPicker
            testid="sidebar-tag-input"
            value={tagQuery}
            onChange={setTagQuery}
            onPick={(v) => {
              onSelect({ ...filter, view: 'all', untagged: undefined, tag: v })
              setTagQuery('')
            }}
            scenario="filter"
            placeholder={t('输入标签筛选…')}
            className="w-full rounded border border-line bg-panel-2 px-2 py-1 text-xs outline-none focus:border-accent"
            panelClassName="absolute top-full left-0 z-40 mt-1 max-h-64 w-64 overflow-y-auto rounded-md border border-line bg-panel p-1 text-xs shadow-lg"
          />
        </div>
        {tags.length === 0 && (
          <p className="px-2 py-1 text-xs leading-5 text-ink-dim">{t('暂无标签（打标后出现）')}</p>
        )}
        {/* 统一标签裁决：`key-val` 前缀分组展示（组标题 = 任务名前缀，行 = 前缀后的值）。
            展示层分组：点击过滤 / 拖放打标仍用完整 value 精确匹配，不改后端契约。 */}
        {groupByTagPrefix(tags, (t) => t.value).map((g) => (
          <div key={g.key || '_plain'} data-testid={`tag-group-${g.key || 'plain'}`}>
            {g.key && (
              <p
                className="mt-2 px-2 pb-1 text-[11px] font-medium tracking-wide text-ink-dim"
                title={t('维度「{key}」（任务名做前缀）', { key: g.key })}
              >
                {g.key}
              </p>
            )}
            {g.entries.map(({ item, value, display }) => (
              <Row
                key={value}
                icon="#"
                label={display}
                title={t('标签「{value}」 · {count} 张', { value, count: item.count })}
                count={item.count}
                active={!!filter.tag && filter.tag === value}
                onClick={() => onSelect({ ...filter, view: 'all', untagged: undefined, tag: value })}
                dropHint={dragActive}
                onDropIds={(ids) => ids.length > 0 && onDropTag(ids, value)}
                testid={`drop-tag-${value}`}
              />
            ))}
          </div>
        ))}
      </div>
      {dragActive && (
        <p
          data-testid="drag-hint"
          className="border-t border-line px-3 py-2 text-[11px] leading-4 text-ink-dim"
        >
          {t('拖到目录 = 移动 · 拖到标签 = 打标 · 拖到底部导出区 = 导出')}
        </p>
      )}
    </div>
  )
}

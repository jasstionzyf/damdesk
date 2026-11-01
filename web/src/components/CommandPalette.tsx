import { useCallback, useEffect, useMemo, useState } from 'react'
import { useI18n } from '../i18n'
import { useNavigate } from 'react-router-dom'
import { Command } from 'cmdk'
import { api, type Capabilities, type SmartFolderDto } from '../lib/api'
import { can, useCapabilities } from '../lib/caps'
import { parseQuery, pushRecent, querySummary, recentIds, stashView, type StashedView } from '../lib/smartFolders'
import { DENSITY_PRESETS, DEFAULT_THUMB, DENSITY_PREF_KEY, clampThumb } from './DensityControl'

/**
 * 当前生效的缩略图档宽：URL `?thumb=` > 本地偏好 > 默认（与图库 useUrlState 同口径）。
 * 只读，不改任何状态。
 */
function currentThumb(): number {
  try {
    const sp = new URLSearchParams(window.location.search)
    const fromUrl = sp.get('thumb')
    if (fromUrl !== null) return clampThumb(fromUrl)
    const raw = localStorage.getItem(DENSITY_PREF_KEY)
    if (raw) return clampThumb(Number(raw))
  } catch {
    /* 无 window / 存储不可用 */
  }
  return DEFAULT_THUMB
}

/**
 * 图库深链（KI-14）：已经在图库时保留既有 URL（选择集 `ids=` / 筛选 / 视图），只 patch 指定键；
 * 从别的页面调用则给干净链接 —— 「找相似」必须带着当前选择才能立即执行。
 */
function galleryDeepLink(patch: Record<string, string>): string {
  const base = new URLSearchParams()
  try {
    const loc = window.location
    if (loc.pathname === '/' || loc.pathname === '/gallery') {
      new URLSearchParams(loc.search).forEach((v, k) => base.set(k, v))
    }
  } catch {
    /* SSR：给干净链接 */
  }
  for (const [k, v] of Object.entries(patch)) base.set(k, v)
  const q = base.toString()
  return q ? `/?${q}` : '/'
}

/**
 * E3 全局命令面板（⌘K / Ctrl+K）：全局动作 + 页面跳转 + 智能文件夹跳转。
 * 「最近使用」置顶排序（localStorage 持久化）；空输入显示全部，输入即模糊过滤（cmdk）。
 */
interface CmdItem {
  id: string
  label: string
  hint?: string
  keywords?: string
  group: string
  run: () => void
}

const PAGES = [
  { to: '/', label: '图库' },
  { to: '/search', label: '搜索' },
  // KI-7：去重不进主导航，命令面板留一条做发现兼容（页面 /dedup 仍可深链）
  { to: '/dedup', label: '去重' },
  { to: '/tag-tasks', label: '打标任务' },
  // t_cafb2e45：标签总览（低频入口，不进主导航；命令面板 + 图库侧栏「标签→全部」两处可达）
  { to: '/tags', label: '标签总览' },
  // 卡 t_4bc7fa24：导出不是页面（全局 Dialog），命令面板里以「操作」出现，不走 page: 前缀
  { to: '/settings', label: '设置' },
]

/**
 * 命令面板里「仅检查重复（不入库）」是否出现（KI-7）：同受 `features.scanExternalPath`
 * 门控，容器形态不出现（不是灰置）。
 */
export function scanCommandVisible(caps: Capabilities | null): boolean {
  return can(caps, 'scanExternalPath')
}

export function CommandPalette({
  open,
  onClose,
  onOpenTaskCenter,
}: {
  open: boolean
  onClose: () => void
  onOpenTaskCenter: () => void
}) {
  const { t } = useI18n()
  const navigate = useNavigate()
  const caps = useCapabilities()
  const [folders, setFolders] = useState<SmartFolderDto[]>([])
  const [recents, setRecents] = useState<string[]>([])
  const [query, setQuery] = useState('')

  useEffect(() => {
    if (!open) return
    setQuery('')
    setRecents(recentIds())
    api.smartFolders().then(setFolders).catch(() => setFolders([]))
  }, [open])

  const go = useCallback(
    (run: () => void, id: string) => {
      pushRecent(id)
      onClose()
      run()
    },
    [onClose],
  )

  const items = useMemo<CmdItem[]>(() => {
    const nav = (to: string) => () => navigate(to)
    const out: CmdItem[] = [
      {
        id: 'action:import',
        label: t('导入图片…'),
        hint: t('顶栏 ＋ 导入'),
        group: t('操作'),
        run: () => window.dispatchEvent(new CustomEvent('dam:open-import')),
      },
      {
        id: 'action:taskcenter',
        label: t('打开处理记录'),
        hint: t('导入/嵌入进度'),
        group: t('操作'),
        run: onOpenTaskCenter,
      },
      // KI-7：库外文件夹只读查重（受能力位门控，容器形态不出现）
      ...(scanCommandVisible(caps)
        ? [
            {
              id: 'action:dedup-scan',
              label: t('仅检查重复（不入库）…'),
              hint: t('扫描库外文件夹，只读'),
              keywords: 'dedup scan 查重 库外',
              group: t('操作'),
              run: () => navigate('/?dialog=dedup-scan'),
            } satisfies CmdItem,
          ]
        : []),
      // 卡 t_4bc7fa24：导出收编为全局 Dialog —— 命令面板给的是「打开导出对话框」这个动作，
      // 不是「前往导出页」（页面已撤销）
      {
        id: 'action:export',
        label: t('导出…'),
        hint: t('全局导出对话框（选择集 / 当前筛选）'),
        keywords: 'export 导出 另存',
        group: t('操作'),
        run: () => navigate('/?dialog=export'),
      },
      {
        id: 'action:view-recent',
        label: t('切换视图：最近导入'),
        group: t('操作'),
        run: () => {
          stashView('recent' satisfies StashedView)
          navigate('/')
        },
      },
      {
        id: 'action:view-all',
        label: t('切换视图：全部图片'),
        group: t('操作'),
        run: () => {
          stashView('all' satisfies StashedView)
          navigate('/')
        },
      },
      // ---- KI-14：找相似档位与网格密度的键盘入口（工具条不再放这些参数） ----
      {
        id: 'action:find-similar-semantic',
        label: t('找相似（语义）：同类的其它图'),
        hint: t('用当前选中图检索，结果进图库临时集合'),
        keywords: 'similar semantic 找相似 语义 相似图',
        group: t('操作'),
        run: () => navigate(galleryDeepLink({ find: 'similar', simMode: 'semantic' })),
      },
      {
        id: 'action:find-similar-precise',
        label: t('找相似（精准）：同一张图的不同版本'),
        hint: t('近重复 ≥98% / 同图变体 ≥90%'),
        keywords: 'similar precise 找相似 精准 重复 变体',
        group: t('操作'),
        run: () => navigate(galleryDeepLink({ find: 'similar', simMode: 'visual_precise' })),
      },
      {
        id: 'action:grid-density',
        label: t('网格密度：切换 小 / 中 / 大'),
        hint: t('当前 {preset}，按下循环下一档', {
        preset: DENSITY_PRESETS.find((p) => p.thumb === currentThumb())?.key ?? t('自定义'),
      }),
        keywords: 'density grid thumb 密度 缩略图 网格',
        group: t('操作'),
        run: () => {
          const cur = currentThumb()
          const idx = DENSITY_PRESETS.findIndex((p) => p.thumb === cur)
          const next = DENSITY_PRESETS[(idx + 1) % DENSITY_PRESETS.length]
          try {
            localStorage.setItem(DENSITY_PREF_KEY, String(next.thumb))
          } catch {
            /* 存储不可用：靠 URL 生效（本次链接仍是真源） */
          }
          navigate(galleryDeepLink({ thumb: String(next.thumb) }))
        },
      },
    ]
    for (const p of PAGES) {
      out.push({ id: `page:${p.to}`, label: t('前往：{name}', { name: t(p.label) }), keywords: p.to, group: t('页面'), run: nav(p.to) })
    }
    for (const f of folders) {
      out.push({
        id: `folder:${f.id}`,
        label: t('智能文件夹：{name}', { name: f.name }),
        keywords: querySummary(parseQuery(f.query)),
        group: t('智能文件夹'),
        run: () => navigate(`/smart/${f.id}`),
      })
    }
    return out
  }, [caps, folders, navigate, onOpenTaskCenter])

  const byId = useMemo(() => new Map(items.map((i) => [i.id, i])), [items])
  const recentItems = recents.map((id) => byId.get(id)).filter((x): x is CmdItem => !!x)
  const recentSet = useMemo(() => new Set(recentItems.map((i) => i.id)), [recentItems])
  const groups = useMemo(() => {
    const rest = items.filter((i) => !recentSet.has(i.id))
    const m = new Map<string, CmdItem[]>()
    for (const i of rest) {
      const list = m.get(i.group) ?? []
      list.push(i)
      m.set(i.group, list)
    }
    return m
  }, [items, recentSet])

  if (!open) return null

  const pick = (item: CmdItem) => go(item.run, item.id)

  return (
    <div
      className="fixed inset-0 z-[80] flex items-start justify-center bg-black/60 p-4 pt-[12vh]"
      role="dialog"
      aria-label={t('命令面板')}
      onClick={onClose}
      onKeyDown={(e) => {
        if (e.key === 'Escape') onClose()
      }}
    >
      <div
        className="w-[560px] overflow-hidden rounded-lg border border-line bg-panel shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <Command label={t("命令面板")} shouldFilter className="[&_[cmdk-group-heading]]:px-3 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2 [&_[cmdk-group-heading]]:text-xs [&_[cmdk-group-heading]]:font-semibold [&_[cmdk-group-heading]]:tracking-wider [&_[cmdk-group-heading]]:text-ink-dim">
          <Command.Input
            autoFocus
            value={query}
            onValueChange={setQuery}
            placeholder={t('搜索命令、页面、智能文件夹…')}
            className="w-full border-b border-line bg-transparent px-4 py-3 text-sm outline-none placeholder:text-ink-dim/60"
          />
          <Command.List className="max-h-[46vh] overflow-y-auto p-2">
            <Command.Empty className="px-3 py-8 text-center text-sm text-ink-dim">
              {t('无匹配命令')}
            </Command.Empty>
            {recentItems.length > 0 && (
              <Command.Group heading={t("最近使用")}>
                {recentItems.map((i) => (
                  <Item key={`recent-${i.id}`} item={i} onPick={pick} />
                ))}
              </Command.Group>
            )}
            {[...groups.entries()].map(([group, list]) => (
              <Command.Group key={group} heading={group}>
                {list.map((i) => (
                  <Item key={i.id} item={i} onPick={pick} />
                ))}
              </Command.Group>
            ))}
          </Command.List>
          <div className="flex items-center gap-4 border-t border-line px-4 py-2 text-xs text-ink-dim">
            <span>{t('↑↓ 选择')}</span>
            <span>{t('↵ 执行')}</span>
            <span>{t('Esc 关闭')}</span>
            <span className="ml-auto">
              {t('全局唤起')} <kbd className="rounded border border-line px-1">⌘K</kbd>
            </span>
          </div>
        </Command>
      </div>
    </div>
  )
}

function Item({ item, onPick }: { item: CmdItem; onPick: (i: CmdItem) => void }) {
  return (
    <Command.Item
      value={`${item.label} ${item.keywords ?? ''}`}
      onSelect={() => onPick(item)}
      className="flex cursor-pointer items-center gap-3 rounded-md px-3 py-2 text-sm text-ink-dim data-[selected=true]:bg-accent-soft data-[selected=true]:text-ink"
    >
      <span className="min-w-0 flex-1 truncate">{item.label}</span>
      {item.hint && <span className="shrink-0 text-xs text-ink-dim/70">{item.hint}</span>}
    </Command.Item>
  )
}

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  LibraryTree,
  chipsOf,
  defaultFilter,
  filterKey,
  type FilterChip,
  type LibraryFilter,
} from '../components/LibraryTree'
import { AssetGrid, type PickMods } from '../components/AssetGrid'
import { Button } from '../components/Button'
import { TextField } from '../components/TextField'
import { DEFAULT_THUMB, DENSITY_PREF_KEY, GRID_GAP, clampThumb } from '../components/DensityControl'
import { InspectorDock, useInspectorDock } from '../components/InspectorDock'
import { SelectionBar } from '../components/SelectionBar'
import { VerbActions } from '../components/VerbActions'
import { useSimilarMode, type SimilarMode } from '../lib/similarMode'
import { useSearchParams } from 'react-router-dom'
import { Lightbox } from '../components/Lightbox'
import { MigrateToLibraryDialog, isMigratable } from '../components/MigrateToLibraryDialog'
import { ShortcutsHelp } from '../components/ShortcutsHelp'
import { PathLink } from '../components/PathLink'
import { EmptyState, ErrorBanner } from '../components/StateViews'
import { api, type AssetView, type BatchOpResult, type DirEntry, type LibraryStats, type TagCount } from '../lib/api'
import { can, reasonFor, useCapabilities } from '../lib/caps'
import { useI18n } from '../i18n'
import * as S from '../lib/selection'
import { clearDragPayload, setDragPayload, takeDragPayload } from '../lib/dnd'
import { deleteWithUndo } from '../lib/deleteFlow'
import { useToast } from '../lib/toast'
import { impactLine, useConfirm, verbCount } from '../lib/confirm'
import { clearStashedView, folderCountOf, parseQuery, peekStashedView, SF_EVENT } from '../lib/smartFolders'
import { clearPendingSet, peekPendingSet } from '../lib/tempSet'
import { saveGallerySnapshot, takeGallerySnapshot } from '../lib/galleryCache'
import { useUrlState, type PrefSpec } from '../lib/urlState'
import { useOverlay } from '../lib/modal'
import {
  loadPersistedFilter,
  loadPersistedQuery,
  savePersistedFilter,
  savePersistedQuery,
  savePersistedSelection,
} from '../lib/persistedGallery'
import {
  GALLERY_URL_DEFAULTS,
  exportFilterOf,
  galleryChipPatchOf,
  galleryClearPatch,
  galleryFilterOf,
  galleryUrlPatchOf,
  smartFolderDroppedDimsOf,
  type GalleryUrlState,
} from '../lib/galleryUrlState'
import { stashExportFilterScope } from '../lib/export'

type Health = 'checking' | 'ok' | 'error'

const PAGE_SIZE = 60

/**
 * 搜索框「应用」静默窗口（C7 / 卡 t_eadc13a3：300ms → 800ms）。
 *
 * 为什么节流的是「应用」而不是「输入」：`query` 写进 URL 是深链/刷新/back 的唯一真源，
 * 必须即时；但让它立刻参与筛选就会在敲字/拼音中间态闪「没有匹配」。
 *
 * 为什么是 800ms（用户口径「输完才搜」）：300ms 在中文逐字输入/逐词输入时照样太短——
 * 打完「雪」停 0.3s 就开始查，再打「山」结果又变，「来不及输全就搜索」。
 * 800ms 覆盖正常打字节奏；急的用户按 Enter 立即搜（不等窗口）。
 * 与 `Search.tsx` 的 450ms 自动重搜不是同一只手感：那边是语义搜索（结果排序会变），
 * 这边是浏览页的关键词收窄，宁慢勿跳。
 */
const QUERY_APPLY_DEBOUNCE_MS = 800

/* ---- URL 单一真源（URL-STATE-1 / 卡 t_4b86ba6c、D-CHAIN-1 卡 t_319596b3）：图库页全部可变状态 ---- */

/** 本地偏好（persistedGallery）：URL 缺键时才补一次，补完写回 URL 并显示可关闭的来源横幅 */
const GALLERY_PREF: PrefSpec<GalleryUrlState> = {
  label: '上次会话',
  load: () => {
    const f = loadPersistedFilter()
    const q = loadPersistedQuery()
    if (!f && !q) return null
    const rec: Partial<GalleryUrlState> = {}
    if (q) rec.q = q
    if (f) {
      rec.tab = f.view
      if (f.dir) rec.dir = f.dir
      if (f.tag) rec.tags = f.tag
      if (f.untagged) rec.untagged = true
      if (f.missing) rec.missing = true
      if (f.broken) rec.broken = true
      if (f.aspect) rec.aspect = f.aspect
      if (f.cameraModel) rec.camera = f.cameraModel
      if (f.takenFrom) rec.takenFrom = f.takenFrom
      if (f.takenTo) rec.takenTo = f.takenTo
      if (f.sort) rec.sort = f.sort
    }
    return rec
  },
  clear: () => {
    savePersistedFilter(defaultFilter)
    savePersistedQuery('')
  },
}

/* ---- 网格密度（UI-LAYOUT / 卡 t_11662290）：URL 单一真源 + 裸链接偏好兜底 ---- */

/** URL key：`?thumb=170`（px）。默认档与 §9.6「≥1280 列宽 ≥170px」对齐 */
const DENSITY_DEFAULTS = { thumb: DEFAULT_THUMB }
/** 偏好只用 localStorage 存「用户默认值」，URL 缺键时才补一次（§9.5，不静默生效） */
const DENSITY_PREF = {
  label: '上次用的网格密度',
  load: () => {
    try {
      const raw = localStorage.getItem(DENSITY_PREF_KEY)
      if (!raw) return null
      return { thumb: clampThumb(Number(raw)) }
    } catch {
      return null
    }
  },
  clear: () => {
    try {
      localStorage.removeItem(DENSITY_PREF_KEY)
    } catch {
      /* 存储不可用：静默 */
    }
  },
}

/* ---- KI-14：找相似档位（lib/similarMode 唯一实现）----
 * 两种意图分属同一条入口：
 * - `semantic`（默认）= 语义相似（W3 标定阈值，行为与以前完全一致）；
 * - `visual_precise` = 精准匹配（同图变体 / 近似重复）。
 * 档位只住在检查器「找相似的」split button（+ 搜索页图搜图入口旁同一组选择器）；
 * 写本地偏好，不写 URL；旧深链 `?simMode=visual_precise` 挂载时读到仍生效（只读）。
 */

/** 断点判定（§9.3：≥1280 三栏常驻；<1280 图库侧栏转抽屉） */
function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(
    () => typeof window !== 'undefined' && window.matchMedia(query).matches,
  )
  useEffect(() => {
    const mq = window.matchMedia(query)
    const on = (e: MediaQueryListEvent) => setMatches(e.matches)
    setMatches(mq.matches)
    mq.addEventListener('change', on)
    return () => mq.removeEventListener('change', on)
  }, [query])
  return matches
}

/** 视口宽度（px）—— 三栏「放得下几列」是算术题，不能用固定断点糊过去 */
function useViewportWidth(): number {
  const [w, setW] = useState(() => (typeof window === 'undefined' ? 0 : window.innerWidth))
  useEffect(() => {
    const on = () => setW(window.innerWidth)
    window.addEventListener('resize', on)
    on()
    return () => window.removeEventListener('resize', on)
  }, [])
  return w
}

/** 固定占用（与 App 的 aside w-52 / LibraryTree w-60 保持一致；检查器已改抽屉，不占常驻宽度） */
const NAV_W = 208
const TREE_W = 240

/**
 * 图库页：两栏 + 右侧非模态抽屉（左库/目录树 + 中网格；检查器是抽屉，不再占宽）。
 * E2：多选模型（单击 / 加选 / 范围选 / 全选）+ 全键盘流（/ 搜索、X 拒选、E 导出、? 帮助）
 * + Lightbox 全屏预览（与网格选中态双向同步）。
 * E4：批量编辑（检查器抽屉多选模式）+ 拖拽语义（目录=移动 / 标签=打标 / 导出区=导出）+ 找相似临时集合。
 */
export function Gallery({ onImport }: { onImport: () => void }) {
  const { t, locale } = useI18n()
  const { toast } = useToast()
  const confirm = useConfirm()
  // WEBCAP：形态能力开关（服务端算，UI 只照着灰置；null = 未加载不灰置）
  const caps = useCapabilities()
  const [health, setHealth] = useState<Health>('checking')
  const [stats, setStats] = useState<LibraryStats | null>(null)
  const [dirs, setDirs] = useState<DirEntry[]>([])
  const [roots, setRoots] = useState<string[]>([])
  const [tagCloud, setTagCloud] = useState<TagCount[]>([])
  /**
   * URL 是唯一真源（q/tab/dir/tags/untagged/missing/broken/aspect/
   * camera/takenFrom/takenTo/sort/open/ids；`rating` 是 KI-11 之前的旧评分键，
   * 解析兼容（静默忽略）但不再写回 —— 见 lib/galleryUrlState.ts）；本地偏好只在 URL 缺该键时兜底一次，
   * 补完立刻写回 URL 并显示可关闭的来源横幅（不静默生效）。
   */
  const ui = useUrlState(GALLERY_URL_DEFAULTS, GALLERY_PREF)
  const { q: qParam, open: openPanel, ids: idsParam } = ui.value

  /** 过滤条件由 URL 派生（纯函数在 lib/galleryUrlState.ts，单测覆盖；开浮层不重拉数据） */
  const filter: LibraryFilter = useMemo(() => galleryFilterOf(ui.value), [ui.value])

  /**
   * KI-16 #7（卡 t_f6d214fb）：`?sf=<id>` = 从侧栏智能文件夹进来。
   *
   * 条件本身在 dir/tags/untagged/missing 上（`smartFolderHref` 写入），`sf` 只承载「来源」：
   *  1. 筛选横幅显示「智能文件夹：<名字> ×」——移除芯片 = 解除绑定，条件作为普通芯片各自保留；
   *  2. 计数对账：保存时记下的 total（`folderCountOf`）与本次加载的 total 不等 → 标「条件已变」。
   * 文件夹被删（深链/别的窗口删）时给「文件夹不存在」，但已带的筛选条件照旧生效（不白屏）。
   */
  const sfId = ui.value.sf
  const [sfFolder, setSfFolder] = useState<{ id: number; name: string; dropped: string[] } | null>(null)
  const [sfMissing, setSfMissing] = useState(false)
  /** SF_EVENT 触发一次重取（侧栏改名/删除后横幅立即跟着变） */
  const [sfTick, setSfTick] = useState(0)
  useEffect(() => {
    const on = () => setSfTick((n) => n + 1)
    window.addEventListener(SF_EVENT, on)
    return () => window.removeEventListener(SF_EVENT, on)
  }, [])
  useEffect(() => {
    if (!sfId) {
      setSfFolder(null)
      setSfMissing(false)
      return
    }
    let stop = false
    api
      .smartFolder(sfId)
      .then((f) => {
        if (stop) return
        setSfFolder({ id: f.id, name: f.name, dropped: smartFolderDroppedDimsOf(parseQuery(f.query)) })
        setSfMissing(false)
      })
      .catch(() => {
        if (stop) return
        setSfFolder(null)
        setSfMissing(true)
      })
    return () => {
      stop = true
    }
  }, [sfId, sfTick])

  /**
   * 关键词输入：显示与提交全部交给基础组件 `<TextField>`（`components/TextField.tsx`）——
   * 那里钉死了 IME 安全（不吞 onChange、不用 compositionend 闩锁）与「父级 key 重建后还焦点」。
   * 页面这边只负责两件事：把已提交值写成 URL `q`（唯一真源），以及记住「有没有没提交的字」用于在途判定。
   */
  const [pendingDraft, setPendingDraft] = useState(qParam)
  /** 已提交关键词（URL `q` 去空白）—— 进请求 + 命中数 + 空态判定的唯一口径 */
  const serverQ = qParam.trim()
  /** 立即落地（Enter / 清空 / 芯片移除 / Esc）：写 URL，拉数据 effect 跟着 serverQ 走 */
  const commitQ = useCallback(
    (v: string) => {
      const next = v.trim()
      if (next !== serverQ) ui.set({ q: next })
    },
    [serverQ, ui.set],
  )

  /**
   * C7（卡 t_eadc13a3）：搜索框「输入即真源、输完才查（800ms 静默）、Enter 立即查」。
   *
   * - `query`（= URL 的 q）随每次输入即时更新 → 深链 / 刷新 / back 语义零改动；
   * - `appliedQuery` 才是喂给服务端的值（`?q=`）→ 敲字/拼音的中间串不参与检索，
   *   不会再闪「没有匹配「xue」」（用户以为图丢了）；
   * - 清空（query 变空）是用户明确动作（Esc / 清空筛选 / 删到空）→ 立即生效不排队，
   *   否则「按了 Esc 还显示 2 张」；Enter = 用户明说「就按这串查」→ 也立即生效。
   *
   * 关键词走**服务端**（`GET /api/library/assets?q=`）——不再是客户端筛选：后者只能筛
   * 已加载的这一页（60 张），全库命中的图只要不在首页就永远搜不到（用户实测「结果不全」）。
   * 所以这里同时决定「什么时候发请求」，不只是「什么时候筛」。
   *
   * `settledQ` 记录**当前 items 对应的关键词**：`serverQ !== settledQ` 说明查询在途，
   * 期间不许出「没有匹配」空态（否则一按 Enter 就先闪一下 0 张，比不搜还吓人）。
   */
  /** 当前 items 对应的关键词（请求落地后写入）；初值 = 首屏关键词，避免首帧误判「查询在途」 */
  const [settledQ, setSettledQ] = useState(() => qParam.trim())
  /**
   * 查询在途：输入框里的字符还没提交（draft ≠ serverQ），或请求已发未回。
   * 两种期间都沿用上一份结果（不清空、不出空态），用户看到的是「还在按旧的筛」而不是「图没了」。
   */
  const queryPending = pendingDraft.trim() !== serverQ || serverQ !== settledQ

  // 选择集：URL ?ids= 优先（深链 / 刷新 / back 都靠 URL 恢复）。
  // 刻意不从 sessionStorage 补：否则一次带选择集的会话会把 ids 写进别人分享的纯筛选链接。
  const [sel, setSel] = useState<S.SelState>(() =>
    idsParam.length > 0
      ? { ids: idsParam, anchor: idsParam[0], active: idsParam[idsParam.length - 1] }
      : S.EMPTY,
  )
  // 选择集写回 URL（write-through）：URL 仍是刷新 / 深链的唯一恢复来源
  useEffect(() => {
    const cur = idsParam
    const same = cur.length === sel.ids.length && cur.every((v, i) => v === sel.ids[i])
    if (!same) ui.set({ ids: sel.ids })
  }, [sel.ids, idsParam, ui.set])

  /**
   * UI-CARRIER-1：导出对话框改成 URL 驱动的单实例（挂在 App 里，`?dialog=export&ids=…`）。
   * 图库只负责「按 E / 拖到导出区」把选择集写进 URL，不再自持一份浮层状态——
   * 否则同一能力会有两个实例（一个在 App、一个在页面），刷新/back 行为必然漂移。
   */
  const exportOverlay = useOverlay('dialog', 'export', ['ids'])
  const exportOpen = exportOverlay.open
  /**
   * DT7 迁入库：同样 URL 驱动（`?dialog=migrate-to-library&ids=…`），单实例挂在下方渲染段。
   * 图库只负责把选择集写进 URL（一个能力一个入口）。
   */
  const migrateOverlay = useOverlay('dialog', 'migrate-to-library', ['ids'])
  const migrateOpen = migrateOverlay.open

  // 浮层：open=lightbox|help 进 URL（打开用 push → 浏览器 back 优先关浮层；关闭用 replace）
  const lightboxOpen = openPanel === 'lightbox'
  const helpOpen = openPanel === 'help'
  const openOverlay = useCallback(
    (name: 'lightbox' | 'help') => ui.setPush({ open: name }),
    [ui.setPush],
  )
  const closeOverlay = useCallback(() => ui.set({ open: '' }), [ui.set])
  /** P1-4：删除二次确认进行中（确认框打开时不响应全局快捷键） */
  const [confirming, setConfirming] = useState(false)

  // ---- UI-LAYOUT：网格密度（URL 真源）+ 三栏断点折叠 ----
  const density = useUrlState(DENSITY_DEFAULTS, DENSITY_PREF)
  const thumb = clampThumb(density.value.thumb)
  const wide = useMediaQuery('(min-width: 1280px)')
  const vw = useViewportWidth()
  /** 图库侧栏开合：≥1280 默认常驻展开，<1280 默认收起（点按钮/抽屉里的关闭再开） */
  const [treeOpen, setTreeOpen] = useState(wide)
  useEffect(() => {
    setTreeOpen(wide)
  }, [wide])
  /**
   * KI-14：密度档位选择（S/M/L）已移出工具条 —— 改在设置页「外观」与命令面板「网格密度」。
   * 图库只保留 thumb 的读取（`?thumb=` 深链 + 偏好兜底），列数仍由 CSS auto-fill 算，无需再测。
   */
  useEffect(() => {
    const t = setTimeout(clearStashedView, 1500)
    // E3 命令面板「切换视图」交接 → 写进 URL（URL 是唯一真源：可分享 / 刷新保留）
    const stashed = peekStashedView()
    if (stashed) ui.set({ tab: stashed })
    return () => clearTimeout(t)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  // UX 持久化：筛选/快速筛选/选择集变化即落盘（裸链接时的偏好兜底；URL 才是真源）
  useEffect(() => savePersistedFilter(filter), [filter])
  useEffect(() => savePersistedQuery(serverQ), [serverQ])
  useEffect(() => savePersistedSelection(sel), [sel])
  const [items, setItems] = useState<AssetView[]>([])
  const [total, setTotal] = useState(0)
  const [pageLoading, setPageLoading] = useState(false)
  const [loadErr, setLoadErr] = useState<string | null>(null)
  const [reloadKey, setReloadKey] = useState(0)
  const loadSeq = useRef(0)
  const searchRef = useRef<HTMLInputElement | null>(null)
  // ---- E1 快照：离页保存 / 回页恢复（已加载数据 + 滚动位置 + 过滤条件） ----
  const [restoreTop, setRestoreTop] = useState<number | null>(null)
  const seeded = useRef(false)
  const rapid = useRef(0)
  const live = useRef({ filter, q: '', items, total, scrollTop: 0 })
  live.current.filter = filter
  live.current.q = serverQ
  live.current.items = items
  live.current.total = total

  // ---- E4 状态 ----
  const [busy, setBusy] = useState(false) // 批量标签/色标/移动进行中
  const [tagsVersion, setTagsVersion] = useState(0) // 标签编辑后递增 → 检查器标签区重取
  const [dragActive, setDragActive] = useState(false) // 网格正在拖拽
  const [dragCount, setDragCount] = useState(0)
  const [exportHover, setExportHover] = useState(false)
  const [similarBusy, setSimilarBusy] = useState(false)
  /**
   * 找相似档位（KI-14）：偏好真源（localStorage），旧深链 ?simMode= 兼容只读。
   * 只影响「找相似的」这一次动作，不影响浏览/筛选。
   */
  const { mode: simMode, setMode: setSimMode } = useSimilarMode()
  /** 临时集合（找相似 / 搜索结果交接）：作为当前视图，可继续多选 / 批量操作 */
  /**
   * 临时集合（找相似 / 搜索结果交接）—— 作为当前视图，可继续多选 / 批量操作。
   * `label` 是横幅文案；`assetId` 只在「以图搜图」时存在（W3 阈值展示要用）。
   */
  const [similarSet, setSimilarSet] = useState<{ label: string; assetId?: number; ids: number[] } | null>(null)
  const [similarItems, setSimilarItems] = useState<AssetView[]>([])
  const [similarLoading, setSimilarLoading] = useState(false)
  /** W3：本次以图搜图生效的相似度下限（null = 未标定模型，后端不过滤） */
  const [similarMin, setSimilarMin] = useState<number | null>(null)
  /** W3：命中相似度表（assetId → cos），检查器里显示"相似度 x%" */
  const [similarScores, setSimilarScores] = useState<Record<number, number>>({})
  /** KI-5：精准模式的分带（assetId → near_duplicate | variant）；语义模式恒空 */
  const [similarBands, setSimilarBands] = useState<Record<number, 'near_duplicate' | 'variant'>>({})
  /**
   * KI-5：本次精准匹配的可见口径（横幅用）。
   * `note` 是阈值/分带人话；`missing` 是「有几张候选没判重向量」—— 不许静默变少。
   */
  const [similarInfo, setSimilarInfo] = useState<{
    mode: 'semantic' | 'visual_precise'
    note: string
    missingVectors: number
  } | null>(null)

  /** KI-5：分带统计（横幅显示「近似重复 N / 同图变体 M」；语义模式恒 0/0） */
  const similarBandCounts = useMemo(() => {
    const bands = Object.values(similarBands)
    const near = bands.filter((b) => b === 'near_duplicate').length
    return { near, variant: bands.length - near }
  }, [similarBands])

  const refreshSidebars = useCallback(() => {
    api.stats().then(setStats).catch(() => {})
    api
      .dirs()
      .then((d) => {
        setDirs(d.dirs)
        setRoots(d.roots)
      })
      .catch(() => {})
    api.tags().then(setTagCloud).catch(() => {})
  }, [])

  const checkHealth = useCallback(() => {
    setHealth('checking')
    api
      .stats()
      .then((s) => {
        setStats(s)
        setHealth('ok')
      })
      .catch(() => setHealth('error'))
  }, [])

  useEffect(() => {
    checkHealth()
    refreshSidebars()
    const onLibChanged = () => {
      refreshSidebars()
      setReloadKey((k) => k + 1)
    }
    const onFocus = () => checkHealth()
    window.addEventListener('dam:library-changed', onLibChanged)
    window.addEventListener('focus', onFocus)
    return () => {
      window.removeEventListener('dam:library-changed', onLibChanged)
      window.removeEventListener('focus', onFocus)
    }
  }, [checkHealth, refreshSidebars])

  // ---- E5 导入即图库：导入期间 2s 轮询进度；未落库文件渲染为网格顶部半透明占位卡，
  // 每处理完一个文件 done 递增 → 轻量合并首页新资产（不整页重载，保持滚动/选择）。 ----
  const [pendingImports, setPendingImports] = useState<string[]>([])
  const importDone = useRef(-1)
  // 占位卡只在默认全库视图显示（筛选/相似视图下占位语义不明）
  const showPending =
    !similarSet &&
    filter.view === 'all' &&
    !filter.untagged &&
    !filter.dir &&
    !filter.tag

  useEffect(() => {
    let stop = false
    let inflight = false
    const tick = async () => {
      if (stop || inflight) return
      inflight = true
      try {
        const st = await api.importStatus()
        if (stop) return
        const p = st.progress
        if (st.running && p) {
          setPendingImports(p.pendingNames ?? [])
          if (importDone.current >= 0 && p.done > importDone.current) {
            const r = await api.assets({ limit: PAGE_SIZE, offset: 0 })
            if (stop) return
            setItems((prev) => {
              const seen = new Set(prev.map((a) => a.assetId))
              const fresh = r.items.filter((a) => !seen.has(a.assetId))
              return fresh.length ? [...fresh, ...prev] : prev
            })
            setTotal((t) => Math.max(t, r.total))
            refreshSidebars()
          }
          importDone.current = p.done
        } else if (importDone.current >= 0) {
          // 批次刚结束：清占位 + 全量刷新一次（含失败文件汇总/总数校正）
          setPendingImports([])
          importDone.current = -1
          setReloadKey((k) => k + 1)
          refreshSidebars()
        }
      } catch {
        /* 后端瞬断：下一轮重试 */
      } finally {
        inflight = false
      }
    }
    void tick()
    const timer = window.setInterval(tick, 2000)
    return () => {
      stop = true
      window.clearInterval(timer)
    }
  }, [refreshSidebars])

  // 过滤条件（E1 多维度叠加 + C7 关键词）→ 查询参数。
  // 条件字段集由 exportFilterOf 单一实现（与导出「当前筛选结果」同一映射，防两份漂移）；
  // 这里只补视图分支（recent 是页面视图不是筛选）、关键词和分页键。
  // 关键词必须进请求：服务端 WHERE 才能覆盖全库（客户端只能筛已加载的这一页）。
  const queryOf = (f: LibraryFilter, offset: number, q: string) => {
    if (f.view === 'recent') return { recent: true, limit: 24, ...(q ? { q } : {}) }
    return { ...exportFilterOf(f, q), limit: PAGE_SIZE, offset } as Parameters<typeof api.assets>[0]
  }

  const isPaginated = filter.view !== 'recent'

  /** 快照签名 = 过滤条件内容 + 关键词（关键词变了就不能拿旧结果回填，否则搜「雪山」出整库） */
  const snapKey = (f: LibraryFilter, q: string) => `${filterKey(f)}\u0000${q}`

  // E1 离页（卸载）保存快照：同过滤条件返回时恢复数据 + 滚动位置
  useEffect(
    () => () => {
      saveGallerySnapshot({
        filterKey: snapKey(live.current.filter, live.current.q),
        items: live.current.items,
        total: live.current.total,
        scrollTop: live.current.scrollTop,
      })
    },
    [],
  )

  // 首页：filter **内容** / reloadKey 变化时重置加载（E1：首挂载优先尝试快照种子，避免回页闪首页/跳顶）
  // UX 持久化：选择集只在 filter 真正变化时清空——初挂载 health 翻转 / 库刷新不清（否则 F5 恢复的选择集被抹掉）
  // 依赖用 filterKey（内容签名）而不是 filter 对象：URL 是唯一真源后，选择集写回（?ids=）也会让
  // ui.value 变成新对象、filter 随之重新 memo，用对象做依赖会在「删除→撤销窗口期」触发无关重取，
  // 把还没落库（10s 后提交）的卡片从服务端读回来、盖掉乐观摘除（D-CHAIN-1 回归，卡 t_58715952）。
  /**
   * C7：请求签名 = 过滤内容 + 关键词。关键词变化必须触发重取（服务端检索），
   * 但它**不算**「筛选条件变化」——不据此清空选择集（用户只是收窄看的范围，
   * 顺手选中的图不该被一次敲键抹掉）。
   */
  const loadSig = useMemo(() => `${filterKey(filter)}\u0000${serverQ}`, [filter, serverQ])
  const lastLoadedFilterKey = useRef<string | null>(null)
  useEffect(() => {
    if (health !== 'ok') return
    const f = live.current.filter
    const q = live.current.q
    const fk = filterKey(f)
    const filterChanged = lastLoadedFilterKey.current !== null && lastLoadedFilterKey.current !== fk
    lastLoadedFilterKey.current = fk
    if (!seeded.current) {
      seeded.current = true
      const snap = takeGallerySnapshot()
      if (snap && snap.filterKey === snapKey(f, q) && snap.items.length > 0) {
        setItems(snap.items)
        setTotal(snap.total)
        setRestoreTop(snap.scrollTop)
        setPageLoading(false)
        setSettledQ(q)
        return
      }
    }
    setRestoreTop(null)
    if (filterChanged) setSel(S.EMPTY)
    const seq = ++loadSeq.current
    setPageLoading(true)
    setLoadErr(null)
    api
      .assets(queryOf(f, 0, q))
      .then((r) => {
        if (seq !== loadSeq.current) return
        setItems(r.items)
        setTotal(r.total)
      })
      .catch((e) => {
        if (seq !== loadSeq.current) return
        setLoadErr(e instanceof Error ? e.message : String(e))
        setItems([])
        setTotal(0)
      })
      .finally(() => {
        if (seq !== loadSeq.current) return
        setPageLoading(false)
        // C7：落地（成功或失败）才算「这份 items 对应 q」——失败也算落地，
        // 否则空态永远不出现，用户看着 0 张却没有任何解释。
        setSettledQ(q)
      })
  }, [loadSig, reloadKey, health])

  const loadMore = useCallback(() => {
    if (pageLoading || !isPaginated || items.length >= total) return
    const seq = ++loadSeq.current
    // E1：连续快速加载（触底风暴）时指数放大页尺寸，降低追加/渲染抖动：60→120→240→480
    const size = Math.min(PAGE_SIZE * 2 ** rapid.current, 480)
    const startedAt = Date.now()
    setPageLoading(true)
    api
      .assets({ ...queryOf(filter, items.length, serverQ), limit: size })
      .then((r) => {
        if (seq !== loadSeq.current) return
        rapid.current = Date.now() - startedAt < 500 ? Math.min(rapid.current + 1, 3) : 0
        setItems((prev) => {
          const seen = new Set(prev.map((a) => a.assetId))
          return [...prev, ...r.items.filter((a) => !seen.has(a.assetId))]
        })
        setTotal(r.total)
      })
      .catch((e) => setLoadErr(e instanceof Error ? e.message : String(e)))
      .finally(() => {
        if (seq === loadSeq.current) setPageLoading(false)
      })
  }, [filter, serverQ, items.length, total, pageLoading, isPaginated])

  // ---- 视图：选择集 / 范围选 / 全选 / Lightbox 翻页都基于「已加载」的显示序 ----
  // C7：图库主列表的关键词由**服务端**筛（items 本身就是命中集），这里不再二次过滤；
  // S.filterByQuery 只留给「找相似临时集合」——那是按 id 现取的临时集合（≤900），
  // 不是全库查询，服务端分页语义不适用。
  const view = useMemo(
    () => (similarSet ? S.filterByQuery(similarItems, serverQ) : items),
    [items, similarSet, similarItems, serverQ],
  )
  const order = useMemo(() => view.map((a) => a.assetId), [view])
  const activeAsset = useMemo(
    () => view.find((a) => a.assetId === sel.active) ?? items.find((a) => a.assetId === sel.active) ?? null,
    [view, items, sel.active],
  )
  const lightboxIndex = useMemo(() => view.findIndex((a) => a.assetId === sel.active), [view, sel.active])
  /** 批量编辑目标（视图序里的选中资产） */
  const batchAssets = useMemo(
    () => sel.ids.map((id) => view.find((a) => a.assetId === id)).filter((a): a is AssetView => !!a),
    [sel.ids, view],
  )
  /** 检查器抽屉是否有内容（单选一张 / 多选批量）——工具条按钮与自动弹出都看它 */
  const hasSelection = !!activeAsset || batchAssets.length > 1
  /**
   * DOCKED：检查器改右侧非模态抽屉（不再常驻占宽）。自动弹出 + 本会话「手动关过就不再自动弹」
   * 的记忆都在这个 hook 里；URL 真源是 `?drawer=inspector`（深链/刷新可恢复）。
   */
  const inspDock = useInspectorDock({ autoOpenKey: 'gallery', hasSelection })
  /**
   * P1-2 筛选横幅数据源：LibraryFilter 派生芯片 + 关键词。
   * query 是页面态（不进 LibraryFilter），但「筛选中」必须把它算进去 —— 否则用户看到
   * 「筛选中：目录 X」却不知道还有关键词在过滤。
   */
  const gridChips = useMemo<FilterChip[]>(() => {
    const list: FilterChip[] = []
    // KI-16 #7：智能文件夹来源芯片排最前（条件芯片紧随其后，各自可单独移除）
    if (sfId) {
      const notes: string[] = []
      if (sfMissing) notes.push(t('文件夹不存在'))
      const saved = folderCountOf(sfId)
      if (saved !== null && !pageLoading && total !== saved) {
        notes.push(t('条件已变：保存时 {saved} 张，现 {total} 张', { saved, total }))
      }
      if (sfFolder?.dropped.length) notes.push(t('已忽略 {list} 条件', { list: sfFolder.dropped.join('/') }))
      const name = sfFolder?.name ?? `#${sfId}`
      list.push({
        field: 'sf',
        label: t('智能文件夹：{name}', { name: name + (notes.length ? t('（{notes}）', { notes: notes.join(t('；')) }) : '') }),
      })
    }
    list.push(...chipsOf(filter))
    if (serverQ) list.push({ field: 'query', label: t('关键词「{q}」', { q: serverQ }) })
    return list
  }, [sfId, sfFolder, sfMissing, pageLoading, total, filter, serverQ, t, locale])

  // ---- EXIF2：相机型号下拉数据源（随侧栏刷新一起更新） ----
  const [cameras, setCameras] = useState<{ model: string; count: number }[]>([])
  useEffect(() => {
    let stop = false
    api
      .cameras()
      .then((r) => {
        if (!stop) setCameras(r.cameras)
      })
      .catch(() => {
        /* 侧栏数据拉取失败不阻塞图库（下拉为空 = 无相机筛选） */
      })
    return () => {
      stop = true
    }
  }, [refreshSidebars])

  // ---- E1 过滤应用 / 芯片移除 / 滚动上报（全部经 URL 写入 → URL 是唯一真源） ----
  const applyFilter = useCallback(
    (f: LibraryFilter) => {
      setSimilarSet(null)
      setSimilarItems([])
      setSel(S.EMPTY)
      setRestoreTop(null)
      ui.set(galleryUrlPatchOf(f))
    },
    [ui.set],
  )
  /** 芯片一键移除：只把该维度写回默认值（其余维度保留），URL 键随之消失 */
  const removeChip = useCallback(
    (field: FilterChip['field']) => {
      // 关键词不在 LibraryFilter 里（是页面态 query），单独清
      if (field === 'query') {
        ui.set({ q: '' })
        return
      }
      ui.set(galleryChipPatchOf(field))
      setSel(S.EMPTY)
      setRestoreTop(null)
    },
    [ui.set],
  )
  const onScrollTop = useCallback((top: number) => {
    live.current.scrollTop = top
  }, [])

  /** 来源提示横幅「清除」：清掉本地偏好 + 把筛选恢复默认（URL 参数随之消失） */
  const clearPrefFilter = useCallback(() => {
    ui.clearPreference()
    setSimilarSet(null)
    setSimilarItems([])
    setSel(S.EMPTY)
    ui.set(galleryClearPatch())
  }, [ui.clearPreference, ui.set])
  /** 横幅里展示的「生效中的筛选」文案（芯片 + 关键词） */
  const prefChips = [
    ...chipsOf(filter).map((c) => c.label),
    serverQ ? t('关键词「{q}」', { q: serverQ }) : '',
  ].filter(Boolean)

  // ---- 拒选（作用于选择集；无选择时作用游标） ----
  const patchItems = useCallback((ids: number[], patch: (a: AssetView) => AssetView) => {
    const set = new Set(ids)
    const apply = (prev: AssetView[]) => prev.map((a) => (set.has(a.assetId) ? patch(a) : a))
    setItems(apply)
    setSimilarItems(apply) // E4：相似临时集合同样打补丁
  }, [])

  /** E4 批量回执 → toast：部分失败逐条列出（不静默） */
  const reportBatch = useCallback(
    (title: string, r: BatchOpResult) => {
      if (r.failCount === 0) {
        toast('success', title)
        return
      }
      const fails = r.results.filter((x) => !x.ok)
      const detail = fails
        .slice(0, 8)
        .map((x) => `#${x.assetId}: ${x.error ?? t('未知错误')}`)
        .join(t('；'))
      const more = fails.length > 8 ? t('（等 {n} 条）', { n: fails.length }) : ''
      toast('error', t('{title} — 成功 {ok} / 失败 {fail}', { title, ok: r.okCount, fail: r.failCount }), `${detail}${more}`)
    },
    [toast],
  )

  const applyReject = useCallback(
    async (rejected: boolean) => {
      const ids = S.targetIds(sel)
      if (ids.length === 0) {
        toast('warning', t('没有可拒选的图片'), t('先选中图片，再按 X'))
        return
      }
      patchItems(ids, (a) => ({ ...a, approvalStatus: rejected ? 'rejected' : 'approved' }))
      try {
        await api.reject(ids, rejected)
        toast('success', rejected ? t('已拒选 {n} 张', { n: ids.length }) : t('已取消拒选 {n} 张', { n: ids.length }))
      } catch (e) {
        toast('error', t('拒选失败'), e instanceof Error ? e.message : String(e))
        setReloadKey((k) => k + 1)
      }
    },
    [sel, patchItems, toast],
  )

  // X：只要目标里有「未拒选」的就整体拒选，否则整体取消拒选（选择集内一致性）
  const toggleReject = useCallback(() => {
    const t = S.targetIds(sel)
    const anyPending = t.some((id) => items.find((a) => a.assetId === id)?.approvalStatus !== 'rejected')
    void applyReject(anyPending)
  }, [sel, items, applyReject])

  // ---- P1-4 删除 / 撤销（软删进回收站 + 二次确认 + 可撤销） ----
  // 落库走 /api/library/delete → svc-library::soft_delete_assets（与去重裁决同一套回收站语义：
  // deleted=1、源文件不动、可恢复）。二次确认用 lib/confirm.tsx（不可逆/高成本才弹），
  // 确认后进 lib/undoQueue 的「延迟提交」通道：窗口期服务端零请求，撤销纯前端回滚。
  // 接口调用 / 文案 / 撤销接线固化在 lib/deleteFlow.ts（可单测），这里只管 UI 状态搬运。
  /** 删除前快照：撤销时纯前端回滚（不发请求） */
  const deleteSnapshot = useRef<{ items: AssetView[]; similar: AssetView[]; total: number; sel: S.SelState } | null>(null)
  const runDelete = useCallback(
    (ids: number[]) => {
      if (ids.length === 0) return
      deleteWithUndo(api, ids, toast, {
        onRemove: (gone) => {
          const set = new Set(gone)
          // 快照必须在摘除前拍
          deleteSnapshot.current = { items, similar: similarItems, total, sel }
          setItems((prev) => prev.filter((a) => !set.has(a.assetId)))
          setSimilarItems((prev) => prev.filter((a) => !set.has(a.assetId)))
          setTotal((t) => Math.max(0, t - gone.length))
          setSel(S.EMPTY)
        },
        onRestore: () => {
          const snap = deleteSnapshot.current
          deleteSnapshot.current = null
          if (!snap) return
          setItems(snap.items)
          setSimilarItems(snap.similar)
          setTotal(snap.total)
          setSel(snap.sel)
        },
        // 窗口期结束、真落库成功后才动侧栏计数并回读对齐分页
        onCommitted: () => {
          refreshSidebars() // 目录/标签/统计计数
          setReloadKey((k) => k + 1)
        },
        // 落库失败：列表已被 onRestore 回滚，再回读一次对齐服务端真值
        onRollback: () => setReloadKey((k) => k + 1),
      })
    },
    [items, refreshSidebars, sel, similarItems, toast, total],
  )

  /** 删除入口（Delete/Backspace 与工具栏按钮共用）：空选择给引导，非空弹二次确认 */
  const requestDelete = useCallback(async () => {
    const ids = S.targetIds(sel)
    if (ids.length === 0) {
      toast('warning', t('没有可删除的图片'), t('先选中图片：单击 / Shift 范围选 / Cmd(Ctrl)+A 全选，再按 Delete'))
      return
    }
    setConfirming(true)
    let ok = false
    try {
      ok = await confirm({
        title: t('删除 {n} 张图片？', { n: ids.length }),
        body: t('图片会移入回收站，源文件仍留在磁盘（不做物理删除）。'),
        impacts: [impactLine(t('移入回收站'), ids.length, t('张'))].filter((x): x is string => Boolean(x)),
        recoverable: t('10 秒内在提示条上可撤销（撤销不发任何请求）；之后可在「回收站」恢复或彻底删除'),
        confirmLabel: verbCount(t('删除'), ids.length),
        danger: true,
      })
    } finally {
      setConfirming(false)
    }
    if (!ok) return
    runDelete(ids)
  }, [confirm, runDelete, sel, toast])

  // ---- E4 批量标签 / 色标 / 移动（显式 ids：检查器用选择集，拖放用拖拽集合） ----
  const runAddTags = useCallback(
    async (ids: number[], values: string[]) => {
      if (ids.length === 0) {
        toast('warning', t('没有可打标的图片'), t('先选中图片，或把图拖到左侧标签上'))
        return
      }
      setBusy(true)
      try {
        const r = await api.addTags(ids, values)
        reportBatch(t('已为 {n} 张加标签「{values}」', { n: r.okCount, values: values.join('、') }), r)
        setTagsVersion((v) => v + 1)
        refreshSidebars() // 侧栏标签计数
      } catch (e) {
        toast('error', t('加标签失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    },
    [refreshSidebars, reportBatch, toast],
  )

  /**
   * UI-UNDO：解绑标签已经由 Inspector 走「乐观移除 + 撤销窗口」，这里只做**落库**：
   * 不弹成功 toast（避免同一动作两条 toast），失败则 toast + 递增 tagsVersion 让标签回来。
   */
  const runRemoveTag = useCallback(
    async (value: string) => {
      if (!activeAsset) return
      try {
        await api.removeTag(activeAsset.assetId, value)
        setTagsVersion((v) => v + 1)
        refreshSidebars()
      } catch (e) {
        toast('error', t('解绑标签「{value}」失败', { value }), e instanceof Error ? e.message : String(e))
        setTagsVersion((v) => v + 1) // 服务端没删掉 → 重取把标签显示回来
      }
    },
    [activeAsset, refreshSidebars, toast],
  )

  const runSetColor = useCallback(
    async (color: string | null) => {
      const ids = S.targetIds(sel)
      if (ids.length === 0) {
        toast('warning', t('没有可设置色标的图片'), t('先选中图片'))
        return
      }
      setBusy(true)
      try {
        await api.setColor(ids, color)
        patchItems(ids, (a) => ({ ...a, colorLabel: color }))
        toast('success', color ? t('已给 {n} 张设色标（{color}）', { n: ids.length, color }) : t('已清除 {n} 张色标', { n: ids.length }))
      } catch (e) {
        toast('error', t('设置色标失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    },
    [sel, patchItems, toast],
  )

  const runMove = useCallback(
    async (ids: number[], targetDir: string) => {
      if (ids.length === 0) {
        toast('warning', t('没有可移动的图片'), t('先选中图片，或把图拖到左侧目录上'))
        return
      }
      // WEBCAP：入口已灰置，但拖拽是浏览器的老 DOM 事件 —— 这里再拦一次，
      // 后端也会 4xx 前置拒绝（三层：UI disabled / 前端拦截 / 服务端预检）
      if (!can(caps, 'moveAsset')) {
        toast('warning', t('当前形态不支持移动文件'), reasonFor(caps, 'moveAsset'))
        return
      }
      // 分隔符无关（Windows `D:\pics` 也要只取末段显示）
      const dirname = targetDir.split(/[\\/]/).filter(Boolean).pop() ?? targetDir
      // UI-UNDO：移动是**磁盘真操作**（不可用 toast 撤销，移回去要手工搬）→ 保留 Confirm，
      // 主按钮带真实数量，默认焦点在取消；影响明细里写清目标目录与回滚方式。
      const ok = await confirm({
        title: t('移动图片文件'),
        body: t('把选中的图片移动到「{dir}/」，磁盘上的文件会真的换目录（库内路径同步更新）。', { dir: dirname }),
        impacts: [
          impactLine(t('移动图片'), ids.length) ?? '',
          t('目标目录：{dir}', { dir: targetDir }),
        ].filter(Boolean),
        recoverable: t('不会删除文件；要撤回需再把这些图移回原目录（同一磁盘内是 rename，不复制数据）'),
        confirmLabel: verbCount(t('移动'), ids.length),
      })
      if (!ok) return
      setBusy(true)
      try {
        const r = await api.moveAssets(ids, targetDir)
        reportBatch(t('已移动 {n} 张到 {dir}/', { n: r.okCount, dir: dirname }), r)
        if (r.okCount > 0) {
          refreshSidebars() // 目录计数变化
          setReloadKey((k) => k + 1) // 当前目录筛选 / absPath 重新拉取
          if (similarSet) {
            // 相似临时集合里的 absPath 变了 → 按 ids 回读
            api.assets({ ids: similarSet.ids }).then((res) => setSimilarItems(res.items)).catch(() => {})
          }
        }
      } catch (e) {
        toast('error', t('移动失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    },
    [caps, confirm, refreshSidebars, reportBatch, similarSet, toast],
  )

  /**
   * E4 找相似的：一键以图搜图 → 结果作为临时集合（可继续批量操作）。
   * KI-14：档位来自检查器 split button（默认用当前偏好档；菜单选档会带 override 立即以新档执行）。
   * - `semantic`（默认）= 语义相似（W3 标定阈值，行为与以前一致）；
   * - `visual_precise` = 精准匹配（同图变体 / 近似重复，后端判重塔 rerank + 0.90/0.98 分带）。
   */
  const findSimilar = useCallback(async (override?: SimilarMode) => {
    const mode: SimilarMode = override ?? simMode
    if (!activeAsset) {
      toast('warning', t('先选中一张图片'), t('检查器「找相似的」以当前选中图为查询'))
      return
    }
    const src = activeAsset.assetId
    setSimilarBusy(true)
    try {
      // ---- 精准匹配（KI-5）：只要极像的图（近重复 / 同图变体） ----
      if (mode === 'visual_precise') {
        const res = await api.searchSimilarPrecise(src, 60)
        const nearPct = Math.round(res.nearThreshold * 100)
        const varPct = Math.round(res.variantThreshold * 100)
        setSimilarBands(Object.fromEntries(res.hits.map((h) => [h.assetId, h.band])))
        setSimilarScores(Object.fromEntries(res.hits.map((h) => [h.assetId, h.score])))
        setSimilarMin(res.variantThreshold)
        // 三种「没结果」的原因必须分开说，不能混成一句「没有相似图」
        if (!res.dedupIndexReady || res.queryMissingDedupVector) {
          setSimilarSet(null)
          setSimilarItems([])
          setSimilarBands({})
          setSimilarScores({})
          setSimilarInfo({
            mode: 'visual_precise',
            note: t('精准匹配需要判重向量（≥{var}% 同图变体 / ≥{near}% 近似重复）', { var: varPct, near: nearPct }),
            missingVectors: res.missingDedupVectors,
          })
          toast(
            'info',
            t('精准匹配暂不可用'),
            res.dedupIndexReady
              ? t('这张图还没有判重向量（先对库跑一次嵌入），暂无法判断是否近重复')
              : t('本库还没算过判重向量（先对库跑一次嵌入），精准匹配无从比较'),
          )
          return
        }
        const ids = res.hits.map((h) => h.assetId)
        const missNote =
          res.missingDedupVectors > 0
            ? t(' · 另有 {n} 张候选缺判重向量，未参与比较', { n: res.missingDedupVectors })
            : ''
        if (ids.length === 0) {
          setSimilarSet(null)
          setSimilarItems([])
          setSimilarBands({})
          setSimilarScores({})
          setSimilarInfo({
            mode: 'visual_precise',
            note: t('精准匹配：≥{var}% 同图变体 / ≥{near}% 近似重复', { var: varPct, near: nearPct }),
            missingVectors: res.missingDedupVectors,
          })
          toast(
            'info',
            t('没有近重复或同图变体'),
            t('候选 {n} 张里没有相似度 ≥ {var}% 的图', { n: res.candidatePool, var: varPct }) + missNote,
          )
          return
        }
        const near = res.hits.filter((h) => h.band === 'near_duplicate').length
        const variant = res.hits.length - near
        const note = t('精准匹配（同图变体 ≥{var}% / 近似重复 ≥{near}%）', { var: varPct, near: nearPct })
        setSimilarInfo({ mode: 'visual_precise', note, missingVectors: res.missingDedupVectors })
        setSimilarSet({ label: t('来自：找相似 · 精准匹配 · 相似于 #{src}', { src }), assetId: src, ids })
        setSimilarLoading(true)
        setSel(S.EMPTY)
        try {
          const full = await api.assets({ ids })
          const rank = new Map(ids.map((id, i) => [id, i]))
          setSimilarItems(
            [...full.items].sort((a, b) => (rank.get(a.assetId) ?? 1e9) - (rank.get(b.assetId) ?? 1e9)),
          )
        } finally {
          setSimilarLoading(false)
        }
        toast(
          'success',
          t('找到 {n} 张极像的图', { n: ids.length }),
          t('近似重复 {near} · 同图变体 {variant}', { near, variant }) + missNote + t(' · 临时集合可继续多选 / 打标 / 导出'),
        )
        return
      }

      // ---- 语义相似（默认，W3 行为不变） ----
      // W3：minScore 缺省 = 后端按模型标定的相似度下限（siglip2 0.72 / dinov3 0.20）；
      // 不能在前端写死一个数——不同嵌入模型 cos 尺度差 3 倍以上。
      // KI-14：mode 显式透传，两档请求体可对账（semantic / visual_precise）。
      const res = await api.searchSimilar(src, 60, undefined, undefined, 'semantic')
      const min = res.minScoreCalibrated ? (res.minScore ?? 0) : null
      // 后端已过滤，这里再兜一次（老后端 / 传了 minScore=0 时保持旧行为）
      const hits = res.hits
        .filter((h) => h.assetId !== src)
        .filter((h) => (res.minScore ? h.score >= res.minScore - 1e-6 : true))
      const ids = hits.map((h) => h.assetId)
      setSimilarBands({})
      setSimilarInfo({
        mode: 'semantic',
        note: min === null ? t('语义相似（当前模型未标定，后端不过滤）') : t('语义相似 ≥ {pct}%', { pct: Math.round(min * 100) }),
        missingVectors: 0,
      })
      if (ids.length === 0) {
        setSimilarSet(null)
        setSimilarItems([])
        setSimilarScores({})
        setSimilarMin(min)
        toast(
          'info',
          t('没有找到足够相似的图'),
          min === null
            ? t('当前嵌入模型没有标定相似度下限，索引里没有可比较的向量——先对库跑一次嵌入')
            : t('这张图在本库里没有相似度 ≥ {pct}% 的图（阈值按模型标定，越低越宽松）', { pct: Math.round(min * 100) }),
        )
        return
      }
      setSimilarSet({ label: t('来自：找相似 · 语义相似 · 相似于 #{src}', { src }), assetId: src, ids })
      setSimilarMin(min)
      setSimilarScores(Object.fromEntries(hits.map((h) => [h.assetId, h.score])))
      setSimilarLoading(true)
      setSel(S.EMPTY)
      try {
        const full = await api.assets({ ids })
        // 按相似度序（hits 顺序）重排
        const rank = new Map(ids.map((id, i) => [id, i]))
        setSimilarItems([...full.items].sort((a, b) => (rank.get(a.assetId) ?? 1e9) - (rank.get(b.assetId) ?? 1e9)))
      } finally {
        setSimilarLoading(false)
      }
      toast(
        'success',
        t('找到 {n} 张相似图', { n: ids.length }),
        min === null
          ? t('结果作为临时集合展示，可继续多选 / 打标 / 导出')
          : t('相似度 ≥ {pct}%（按当前嵌入模型标定）', { pct: Math.round(min * 100) }) + t(' · 临时集合可继续多选 / 打标 / 导出'),
      )
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      toast(
        'error',
        t('找相似的失败'),
        msg.includes('no_vector_index') || msg.includes('provider')
          ? t('{msg} —— 请先在设置页配置嵌入模型并对库跑一次嵌入', { msg })
          : msg,
      )
    } finally {
      setSimilarBusy(false)
    }
  }, [activeAsset, simMode, toast])

  /**
   * KI-14：命令面板「找相似（语义 / 精准）」的一次性交接 —— `?find=similar&simMode=…`。
   * URL 驱动（不靠跨页事件）：页内直接生效、跨页导航后挂载即消费；消费后 replace 掉两个键，
   * 不留历史、不被刷新重放。档位会同时写偏好（setSimMode）之后一律按偏好走。
   *
   * 注意：挂载瞬间 `?ids=` 的选择集可能还没读完（items 异步），此时不能直接执行（会误报「先选中一张图片」），
   * 先记 pending，等选择落定（有 activeAsset）或加载结束（无选中 → 给正常提示）再执行一次。
   */
  const [searchParams, setSearchParams] = useSearchParams()
  const findOnce = searchParams.get('find')
  const pendingFindMode = useRef<SimilarMode | null>(null)
  useEffect(() => {
    if (findOnce !== 'similar') return
    const m: SimilarMode = searchParams.get('simMode') === 'visual_precise' ? 'visual_precise' : 'semantic'
    const next = new URLSearchParams(searchParams)
    next.delete('find')
    next.delete('simMode')
    setSearchParams(next, { replace: true })
    setSimMode(m)
    pendingFindMode.current = m
    // 只消费一次：处理完即可，参数已从 URL 移除
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [findOnce])
  useEffect(() => {
    if (!pendingFindMode.current) return
    const fire = () => {
      const m = pendingFindMode.current
      if (!m) return
      pendingFindMode.current = null
      void findSimilar(m)
    }
    // 选择集已落定 → 立刻执行；否则最多等 800ms 让 `?ids=` 恢复（无选中就按正常提示走）
    if (activeAsset) {
      fire()
      return
    }
    const t = setTimeout(fire, 800)
    return () => clearTimeout(t)
  }, [activeAsset, findSimilar])

  // ---- E4 拖拽（网格 → 目录=移动 / 标签=打标 / 导出区=导出） ----
  const onDragAssets = useCallback((ids: number[]) => {
    setDragPayload(ids)
    setDragCount(ids.length)
    setDragActive(true)
    setExportHover(false)
  }, [])

  const onGridDragEnd = useCallback(() => {
    clearDragPayload()
    setDragActive(false)
    setExportHover(false)
  }, [])

  const onDropMove = useCallback(
    (ids: number[], dir: string) => {
      setDragActive(false)
      void runMove(ids, dir)
    },
    [runMove],
  )

  const onDropTag = useCallback(
    (ids: number[], name: string) => {
      setDragActive(false)
      void runAddTags(ids, [name])
    },
    [runAddTags],
  )

  const clearSimilar = useCallback(() => {
    setSimilarSet(null)
    setSimilarItems([])
    setSimilarScores({})
    setSimilarBands({})
    setSimilarInfo(null)
    setSel(S.EMPTY)
  }, [])

  // 搜索页 → 图库 的临时集合交接（卡 t_dc97b141）：搜索结果作为当前视图打开，
  // 打标/移动/色标/导出等批量动作交给图库检查器（不另写一套实现）。
  const consumedPendingSet = useRef(false)
  useEffect(() => {
    if (consumedPendingSet.current) return
    consumedPendingSet.current = true
    const pending = peekPendingSet()
    if (!pending) return
    clearPendingSet()
    setSimilarSet({ label: pending.label, ids: pending.ids })
    setSel(S.EMPTY)
    setSimilarLoading(true)
    api
      .assets({ ids: pending.ids })
      .then((res) => {
        const rank = new Map(pending.ids.map((id, i) => [id, i]))
        setSimilarItems(
          [...res.items].sort((a, b) => (rank.get(a.assetId) ?? 1e9) - (rank.get(b.assetId) ?? 1e9)),
        )
      })
      .catch((e) => toast('error', t('打开搜索结果失败'), e instanceof Error ? e.message : String(e)))
      .finally(() => setSimilarLoading(false))
  }, [toast])

  // ---- 选择交互 ----
  const pick = useCallback(
    (a: AssetView, mods: PickMods) => {
      setSel((s) => {
        if (mods.shift) return S.range(order, s, a.assetId)
        if (mods.meta) return S.toggle(s, order, a.assetId)
        return S.single(a.assetId)
      })
    },
    [order],
  )

  const openLightbox = useCallback(
    (a: AssetView) => {
      setSel((s) => (s.ids.includes(a.assetId) ? { ...s, active: a.assetId } : S.single(a.assetId)))
      openOverlay('lightbox')
    },
    [openOverlay],
  )

  // Lightbox 翻页 → 网格选中态同步（多选时只移动游标，保留选择集，便于连续批打分）
  const onLightboxIndex = useCallback(
    (i: number) => {
      const a = view[i]
      if (!a) return
      setSel((s) =>
        s.ids.length > 1 && s.ids.includes(a.assetId) ? { ...s, active: a.assetId } : S.single(a.assetId),
      )
    },
    [view],
  )

  const openExport = useCallback(() => {
    const ids = S.targetIds(sel)
    if (ids.length === 0) {
      toast('warning', t('先选择要导出的图片'), t('单击选中 / Shift 范围选 / Cmd(Ctrl)+A 全选，然后按 E 导出'))
      return
    }
    // 「当前筛选结果」范围随开随寄存（卡 t_4bc7fa24）：Dialog 用同一份 filter 计数与导出。
    // C7：关键词必须一起寄存 —— 图库按「雪山」筛出 2 张却导出整库是最坏的数据事故。
    // recent 视图 exportFilterOf 返回 null → 清空寄存（Dialog 只给选择集范围，不虚造筛选）。
    const ef = exportFilterOf(filter, serverQ)
    stashExportFilterScope(ef ? { label: t('图库当前筛选'), filter: ef } : null)
    // 选择集随浮层进 URL（?dialog=export&ids=…）：刷新/分享/back 都还原同一次导出
    exportOverlay.openOverlay({ ids: ids.join(',') })
  }, [sel, toast, exportOverlay, filter, serverQ])

  /**
   * KI35-P3：能力面板（送去处理）的「当前筛选结果」范围寄存体 —— 与导出同一份映射
   * （exportFilterOf 单一实现），条件漂移/键不一致这类事故在这里没有第二份可漂移的口径。
   */
  const processFilterScope = useCallback((): Parameters<typeof stashExportFilterScope>[0] => {
    const ef = exportFilterOf(filter, serverQ)
    return ef ? { label: t('图库当前筛选'), filter: ef } : null
  }, [filter, serverQ])

  /**
   * DT7 迁入库入口（工具条唯一入口）。
   *
   * 禁用判据（按钮与浮层内各自算一遍，口径同源 `isMigratable`）：
   *  - 无选择集 → 先选图；
   *  - 已读到能力开关但没有可写库根 → 后端一定会 400，界面先灰；
   *  - 选中项**全部已知且都不可迁**（echo：已 copyin / 失联 / 缺指纹）→ 灰并说明原因；
   *  - 选区里还有不在当前视图的项（深链/临时集合）→ 不拦，交给后端逐条 skip。
   */
  const migrateBlockReason = useMemo(() => {
    if (!hasSelection) return t('先选择要迁入库的图片（单击 / Shift 范围选 / Cmd+A 全选）')
    const root = (caps?.roots ?? []).find((r) => r.writable)?.path ?? ''
    if (caps && !root) return t('没有可写的库根：先在设置里配置库目录并确认可写')
    const allKnown = batchAssets.length === sel.ids.length
    if (allKnown && batchAssets.length > 0 && !batchAssets.some(isMigratable)) {
      return t('选中项都无需迁移（已是库内保管 / 失联 / 缺内容指纹）')
    }
    return ''
  }, [hasSelection, caps, batchAssets, sel.ids.length])

  const openMigrate = useCallback(() => {
    // 无选择集时按钮已灰置（见 migrateBlockReason），这里不重复拦：
    // 浮层对空选择集自带兜底文案（「没有选择集」），少一条到不了的分支
    migrateOverlay.openOverlay({ ids: S.targetIds(sel).join(',') })
  }, [sel, migrateOverlay])

  // ---- 全局键位（图库网格上下文） ----
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null
      const typing =
        !!el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT' || el.isContentEditable)
      if (typing) {
        if (e.key === 'Escape') {
          // Esc 的清空语义由 <TextField> 自己完成（flush('') + blur）；这里对其它原生输入只做失焦
          el?.blur()
        }
        return
      }
      // 浮层打开时键位归浮层（Lightbox / 导出 / 迁入库 / 帮助 / 删除确认 各自处理）
      if (lightboxOpen || exportOpen || migrateOpen || helpOpen || confirming) return

      const mod = e.metaKey || e.ctrlKey
      const k = e.key

      if (k === '/' || k === '?') {
        e.preventDefault()
        if (k === '?') openOverlay('help')
        else {
          searchRef.current?.focus()
          searchRef.current?.select()
        }
        return
      }
      if (mod && (k === 'a' || k === 'A')) {
        e.preventDefault()
        setSel(S.all(order))
        toast('info', t('已全选当前视图 {n} 张', { n: order.length }), t('接着按 E 导出、X 拒选、Esc 清空'))
        return
      }
      if (k === 'Escape') {
        // DOCKED：检查器抽屉开着时 Esc 归它（本层不抢，让它自己的 handler 关掉）
        if (inspDock.open) return
        e.preventDefault()
        if (pendingDraft.trim() || serverQ) commitQ('')
        else setSel(S.EMPTY)
        return
      }
      if (k === 'e' || k === 'E') {
        e.preventDefault()
        openExport()
        return
      }
      // Delete / Backspace：删除选择集（二次确认 → 软删进回收站）
      if (k === 'Delete' || k === 'Backspace') {
        e.preventDefault()
        requestDelete()
        return
      }
      if (k === 'ArrowLeft' || k === 'ArrowRight') {
        e.preventDefault()
        setSel((s) => S.move(s, order, k === 'ArrowLeft' ? -1 : 1, e.shiftKey))
        return
      }
      if (k === 'ArrowUp' || k === 'ArrowDown') {
        // 网格是 auto-fill，列数随窗口变化：上下移动语义不明确，仅 Lightbox 内启用
        e.preventDefault()
        return
      }
      if (k === 'Enter') {
        e.preventDefault()
        if (activeAsset) openLightbox(activeAsset)
        return
      }
      if (k === 'x' || k === 'X') {
        e.preventDefault()
        toggleReject()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [
    lightboxOpen,
    exportOpen,
    helpOpen,
    confirming,
    inspDock.open,
    order,
    pendingDraft,
    commitQ,
    serverQ,
    activeAsset,
    openExport,
    openLightbox,
    toggleReject,
    requestDelete,
    toast,
  ])

  const hasMore = isPaginated && items.length < total
  const empty = health === 'ok' && stats !== null && stats.assets === 0

  // KI-10：卡片右键菜单（只给路径相关动作；路径本体与按钮仍由 <PathLink> 渲染，
  // 这里只负责「贴在哪里」。点页面其它地方 / 滚动 / Esc 都关掉。）
  const [ctxMenu, setCtxMenu] = useState<{ asset: AssetView; x: number; y: number } | null>(null)
  useEffect(() => {
    if (!ctxMenu) return
    const close = () => setCtxMenu(null)
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close()
    }
    window.addEventListener('click', close)
    window.addEventListener('scroll', close, true)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('click', close)
      window.removeEventListener('scroll', close, true)
      window.removeEventListener('keydown', onKey)
    }
  }, [ctxMenu])

  // 批量动作（都作用于选择集）：KI-16 #5 硬约束 —— 容器与文案顺序统一由共用
  // `<SelectionBar>` 出（图库 / 搜索同一份实现），这里只提供按钮本体。
  const batchActions = (
    <>
      {/* KI35-P3：能力入口「送去处理」—— 打开能力面板（能力/方案/范围都在面板里选） */}
      <VerbActions ids={sel.ids} filterScope={processFilterScope} />
      <Button
        data-testid="reject-button"
        title={t('拒选 / 取消拒选选择集（快捷键 X）')}
        onClick={toggleReject}
      >
        {t('拒选 (X)')}
      </Button>
      <Button data-testid="export-button" title={t('导出选择集（快捷键 E）')} onClick={openExport}>
        {t('导出 (E)')}
      </Button>
      {/* DT7：迁入库（reference → copyin）——唯一入口；不可用时灰置 + 原因（不隐藏，免得用户找不到） */}
      <Button
        data-testid="migrate-button"
        disabled={!!migrateBlockReason}
        title={migrateBlockReason || t('把选中图片复制进库根并切为库内保管（源文件默认保留）')}
        onClick={openMigrate}
      >
        {t('迁入库')}
      </Button>
      {/* P1-4：删除入口（此前只能 X 拒选，用户找不到「删除」） */}
      <Button
        data-testid="delete-button"
        tone="danger"
        title={t('删除选择集（快捷键 Delete / Backspace）：移入回收站，可撤销')}
        onClick={requestDelete}
      >
        {t('删除 (Del)')}
      </Button>
    </>
  )
  // 工具条常驻项：快捷键速查 + 检查器门（详情面自己的门，不承担其他能力）
  const toolbar = (
    <>
      <Button data-testid="help-button" title={t('快捷键速查（?）')} onClick={() => openOverlay('help')}>
        {t('? 快捷键')}
      </Button>
      {/* DOCKED：检查器的唯一手动入口 —— 选中即自动滑出，这里用于「关掉后再打开」（一个能力一个入口，
          详情只在抽屉里看；Lightbox 的 I 信息面板已删） */}
      <Button
        data-testid="inspector-toggle"
        active={inspDock.open}
        disabled={!hasSelection}
        title={hasSelection ? t('详情面板（元数据 / 标签 / 找相似）') : t('先选中一张图')}
        onClick={inspDock.toggle}
      >
        {t('详情')}
      </Button>
      {/*
        KI-14（工具条三分类硬规则）：工具条只放**对选择集生效的批量动作**（在 SelectionBar 里）+ 本行
        这两个「常驻门」。已经移出：
        - 找相似档位开关 → 检查器「找相似的」split button（同类的其它图 / 同一张图的不同版本）；
        - 网格密度 S/M/L → 设置页「外观」+ 命令面板「网格密度」（?thumb= 深链仍生效）。
        新增控件归位前先查 known-issues.md 的三分类表，别再往这里塞参数。
      */}
    </>
  )

  const searchSlot = (
    <span className="flex items-center gap-2">
      <TextField
        ref={searchRef}
        data-testid="library-search"
        focusKey="gallery-keyword"
        value={qParam}
        commitDelayMs={QUERY_APPLY_DEBOUNCE_MS}
        onCommit={commitQ}
        onDraftChange={setPendingDraft}
        placeholder={t('关键词筛选文件名 / 标题 / 描述…（/ 聚焦，Enter 立即搜）')}
        spellCheck={false}
        className="w-56 rounded border border-line bg-panel-2 px-2 py-1 text-xs outline-none focus:border-accent"
      />
      {serverQ && (
        <span
          className="text-xs text-ink-dim"
          data-testid="filter-count"
          title={t('已加载 / 命中（服务端全库命中总数）')}
        >
          {/* C7：关键词走服务端后 total = 全库命中数，loaded 才是已取回的页内条数。
              旧口径「已载/已加载」两个都是本地数，搜索时恒等，读者看不出来还在翻页。 */}
          {t('已载 {loaded} / 命中 {total}', { loaded: view.length, total })}
        </span>
      )}
    </span>
  )

  // 中栏内容
  let center: React.ReactNode
  if (health === 'checking') {
    center = <div className="flex flex-1 items-center justify-center text-sm text-ink-dim">{t('连接 dam-web-api…')}</div>
  } else if (health === 'error') {
    center = (
      <div className="flex flex-1 flex-col overflow-y-auto">
        <ErrorBanner
          title={t('无法连接后端服务')}
          detail={t("GET /api/library/stats 失败：dam-web-api 未启动或网络不通。图库数据、导入与后续搜索均依赖后端。")}
          remedies={[
            t('本机部署：cd deploy && docker compose up -d，然后 curl 127.0.0.1:8787/healthz 验证'),
            t('开发模式：确认 vite 已启动且 /api 反代指向 127.0.0.1:8787'),
            t('仍失败时查看容器日志：docker logs dam-web-api'),
          ]}
          onRetry={checkHealth}
        />
      </div>
    )
  } else if (empty) {
    center = (
      <EmptyState
        icon="▣"
        title={t('图库还是空的')}
        desc={t('从导入你的第一个图片目录开始。支持原地索引（不复制文件）或复制入库两种方式。') + (stats ? t(' 当前后端：{n} 张图片。', { n: stats.assets }) : '')}
        actions={
          <Button variant="primary" size="md" onClick={onImport}>
            {t('导入图片')}
          </Button>
        }
        hint={t("导入目录必须是 dam-web-api 所在机器上的绝对路径；进度在右上角处理记录查看")}
      />
    )
  } else {
    center = (
      <div className="flex min-h-0 min-w-0 flex-1 flex-col" data-testid="gallery-center">
        {/* 来源提示（UI 规范三.12）：持久化筛选生效时必须可见、可关、带命中数 —— 不许静默生效 */}
        {ui.showPrefBanner && prefChips.length > 0 && (
          <div
            data-testid="pref-filter-banner"
            className="flex flex-wrap items-center gap-2 border-b border-amber-600/40 bg-amber-500/10 px-4 py-1.5 text-xs"
          >
            <span className="text-amber-300">↺ {t('已在按{label}（本地偏好）筛选：', { label: t(ui.prefLabel) })}</span>
            <span className="text-ink">{prefChips.join(' · ')}</span>
            <span className="text-ink-dim" data-testid="pref-filter-hits">
              {/* C7：total 是服务端全库命中数（关键词也进 WHERE），不再按已加载条数撒谎 */}
              {t('命中 {n} 张', { n: total })}
            </span>
            <Button data-testid="pref-filter-clear" onClick={clearPrefFilter}>
              {t('清除')}
            </Button>
            <Button
              variant="icon"
              size="none"
              data-testid="pref-filter-dismiss"
              aria-label={t('关闭来源提示')}
              title={t('只关闭提示，筛选继续生效')}
              onClick={ui.dismissPref}
              className="ml-auto h-6 w-6 border-0 bg-transparent"
            >
              ×
            </Button>
          </div>
        )}
        {loadErr && (
          <div className="border-b border-red-600/40 bg-red-950/30 px-4 py-2 text-xs text-red-300">
            {t('加载失败：{err}', { err: loadErr })}
          </div>
        )}
        {/* E4：找相似临时集合横幅（可关闭，集合内可继续批量操作） */}
        {similarInfo?.mode === 'visual_precise' && !similarSet && (
          <div
            className="flex items-center gap-2 border-b border-line bg-accent-soft/30 px-4 py-1.5 text-xs"
            data-testid="similar-precise-note"
          >
            <span className="text-accent">◈</span>
            <span className="text-ink">{similarInfo.note}</span>
            {similarInfo.missingVectors > 0 && (
              <span className="text-amber-300" data-testid="similar-missing-vectors">
                {t('另有 {n} 张候选缺判重向量（未参与比较）', { n: similarInfo.missingVectors })}
              </span>
            )}
            <Button
              variant="icon"
              data-testid="similar-precise-dismiss"
              aria-label={t('关闭精准匹配提示')}
              onClick={() => setSimilarInfo(null)}
              className="ml-auto"
            >
              ×
            </Button>
          </div>
        )}
        {similarSet && (
          <div
            className="flex items-center gap-2 border-b border-line bg-accent-soft/40 px-4 py-1.5 text-xs"
            data-testid="similar-banner"
          >
            <span className="text-accent">◈</span>
            <span>
              {similarSet.label} · {similarLoading ? t('加载中…') : t('{n} 张', { n: view.length })}
              {similarMin !== null && similarSet.assetId != null && (
                <span className="ml-1 text-ink-dim">
                  · {t('相似度 ≥ {pct}%', { pct: Math.round(similarMin * 100) })}
                </span>
              )}
              {similarInfo && (
                <span className="ml-2 text-ink-dim" data-testid="similar-mode-note">
                  · {similarInfo.note}
                </span>
              )}
              {similarInfo?.mode === 'visual_precise' && similarBandCounts.near + similarBandCounts.variant > 0 && (
                <span className="ml-2 text-ink" data-testid="similar-band-counts">
                  · {t('近似重复 {near} 张 / 同图变体 {variant} 张', { near: similarBandCounts.near, variant: similarBandCounts.variant })}
                </span>
              )}
              {similarInfo?.missingVectors ? (
                <span className="ml-2 text-amber-300" data-testid="similar-missing-vectors">
                  · {t('{n} 张候选缺判重向量（未参与比较）', { n: similarInfo.missingVectors })}
                </span>
              ) : null}
              <span className="ml-2 text-ink-dim">{t('（临时集合，可继续多选 / 打标 / 拖拽 / 导出）')}</span>
            </span>
            <Button
              data-testid="similar-clear"
              onClick={clearSimilar}
              title={t('退出临时集合，回到当前筛选')}
              className="ml-auto"
            >
                          {t('× 返回')}
                      </Button>
          </div>
        )}
          {/* 来源提示（§9.5）：偏好在裸链接上兜底生效时必须可见可关，不得静默 */}
          {density.showPrefBanner && (
            <div
              data-testid="density-pref-banner"
              className="flex shrink-0 items-center gap-2 border-b border-accent/40 bg-accent-soft/30 px-4 py-1.5 text-xs text-ink"
            >
              <span className="text-accent">◈</span>
              <span>
                {t('{label}：缩略图 {thumb}px（偏好，非链接自带）', { label: t(density.prefLabel), thumb })}
              </span>
              <Button
                data-testid="density-pref-clear"
                onClick={() => {
                  // 只清偏好 + 回默认档：不要再写 localStorage（写回会立刻把偏好「复活」）
                  density.clearPreference()
                  density.set({ thumb: DEFAULT_THUMB })
                }}
              >
                {t('清除并回到默认')}
              </Button>
              <Button
                variant="icon"
                size="none"
                aria-label={t('关闭来源提示')}
                data-testid="density-pref-dismiss"
                onClick={density.dismissPref}
                className="ml-auto h-6 w-6 border-0 bg-transparent"
              >
                ×
              </Button>
            </div>
          )}
          <ExifFilterBar filter={filter} cameras={cameras} onChange={applyFilter} busy={false} />
          {/* KI-16 #5 / KI-14：批量动作条 = 共用 SelectionBar（与搜索页同一份实现，
              文案顺序与「已加载 N 张」口径一致，禁止各页自拼按钮组） */}
          <SelectionBar
            loaded={view.length}
            selected={sel.ids.length}
            total={similarSet ? view.length : filter.view === 'recent' ? view.length : total}
            onSelectAll={() => setSel(S.all(order))}
            onClear={() => setSel(S.EMPTY)}
            actions={batchActions}
            trailing={
              <span className="ml-auto text-ink-dim/70" data-testid="gallery-batch-hint">
                {sel.ids.length === 0
                  ? t('先选中图片（单击 / ⌘Ctrl+A 全选）再执行批量动作')
                  : t('X 拒选 · E 导出 · Delete 删除 · ⌘/Ctrl+A 全选 · 详情在右侧抽屉')}
              </span>
            }
          />
          <AssetGrid
            key={filterKey(filter)}
            resetSignal={loadSig}
            pendingState={
              view.length === 0 && queryPending ? (
                /* 在途（静默窗口未到 / 请求未回）不出空态：沿用上一份结果，首屏则显示这一行 */
                <div
                  data-testid="gallery-search-pending"
                  className="flex flex-1 items-center justify-center text-sm text-ink-dim"
                >
                  {t('正在检索「{q}」…', { q: pendingDraft.trim() || serverQ })}
                </div>
              ) : undefined
            }
            emptyState={
              view.length === 0 && serverQ && !queryPending && !loadErr ? (
                <EmptyState
                  icon="⌕"
                  title={t('没有匹配「{q}」的图片', { q: serverQ })}
                  desc={
                    similarSet
                      ? t('临时集合（{n} 张）里没有文件名/标题/描述含这串字的图片；Esc 清空筛选。', { n: similarItems.length })
                      : t('全库检索（文件名 / 标题 / 描述 / 路径）没有命中；Esc 清空筛选。')
                  }
                  actions={<Button onClick={() => commitQ('')}>{t('清空筛选')}</Button>}
                  hint={t("语义检索（按画面内容找图）在「搜索」页；这里只按文字匹配")}
                />
              ) : undefined
            }
            assets={view}
            thumbPx={thumb}
            total={similarSet ? view.length : filter.view === 'recent' ? view.length : total}
            selectedIds={sel.ids}
            activeId={sel.active}
            onPick={pick}
            onOpen={openLightbox}
            hasMore={similarSet ? false : hasMore}
            loading={similarSet ? similarLoading : pageLoading}
            onLoadMore={loadMore}
            toolbar={toolbar}
            searchSlot={searchSlot}
            onDragAssets={onDragAssets}
            onDragEnd={onGridDragEnd}
            chips={similarSet ? [] : gridChips}
            pendingNames={showPending ? pendingImports : undefined}
            onRemoveChip={removeChip}
            onClearFilter={() => applyFilter(defaultFilter)}
            restoreScrollTop={restoreTop}
            onScrollTop={onScrollTop}
            onAssetContextMenu={(a, e) => setCtxMenu({ asset: a, x: e.clientX, y: e.clientY })}
          />

      </div>
    )
  }

  // 左库面板（§9.3 / 卡 t_11662290）：≥1280 三栏常驻；<1280 转抽屉（可关，Esc 优先关它）。
  // 抽屉只覆盖网格左侧、不锁整页滚动 —— 选完目录即可继续看图。
  // DOCKED：检查器不再是常驻第三栏（改右侧非模态抽屉），侧栏树的让位算术里只算导航 + 树 + 网格，
  // 主区宽度不再被检查器吃掉（1280 视口：1920-208-240 ≈ 832 给网格）。
  const minGridW = 4 * DEFAULT_THUMB + 3 * GRID_GAP
  const treeFits = vw >= NAV_W + TREE_W + minGridW
  const treeInline = treeOpen && wide && treeFits
  const treeDrawerOpen = treeOpen && (!wide || !treeFits)
  // 抽屉开合（<1280 或三栏放不下时）：Esc 优先关抽屉（比"清空筛选"更靠前）
  // DOCKED：检查器抽屉开着时，Esc 先归检查器（优先级：Lightbox > 对话框 > 检查器 > 树抽屉 > 清空筛选）
  useEffect(() => {
    if (!treeDrawerOpen || inspDock.open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      e.preventDefault()
      e.stopPropagation()
      setTreeOpen(false)
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [treeDrawerOpen, inspDock.open])

  const tree = (
    <LibraryTree
      stats={stats}
      dirs={dirs}
      roots={roots}
      tags={tagCloud}
      filter={filter}
      dragActive={dragActive}
      onSelect={applyFilter}
      onRefreshDirs={refreshSidebars}
      onDropMove={onDropMove}
      onDropTag={onDropTag}
    />
  )

  return (
    <div className="flex min-h-0 flex-1" data-testid="gallery-layout" data-wide={wide ? '1' : '0'}>
      {treeInline && (
        <div data-testid="tree-inline" className="flex h-full shrink-0">
          {tree}
        </div>
      )}
      {treeDrawerOpen && (
        <div className="fixed inset-y-0 left-0 z-40 flex shadow-2xl" data-testid="tree-drawer">
          {tree}
          <Button
            variant="icon"
            size="none"
            data-testid="tree-drawer-close"
            aria-label={t('关闭库面板')}
            title={t('关闭库面板（Esc）')}
            onClick={() => setTreeOpen(false)}
            className="w-8 shrink-0 rounded-none border-0 border-y border-r border-line bg-panel text-ink hover:bg-line"
          >
            ‹
          </Button>
        </div>
      )}
      {/* 侧栏收起后的展开把手：<1280 是抽屉入口，≥1280 是手动折叠/为检查器让位后的还原入口 */}
      {!treeInline && (
        <Button
          variant="icon"
          size="none"
          data-testid="tree-toggle"
          aria-label={t('展开库面板')}
          aria-expanded={false}
          title={t('展开库面板（库 / 目录 / 标签）')}
          onClick={() => setTreeOpen(true)}
          className="w-8 shrink-0 rounded-none border-0 border-r border-line bg-panel text-xs text-ink hover:bg-panel-2"
        >
          ›
        </Button>
      )}
      {center}
      {!empty && (
        <InspectorDock
          open={inspDock.open}
          onClose={inspDock.close}
          /* DOCKED：上层浮层开着时挂起抽屉的 Esc（一次 Esc 只关一层，且先关上面那层） */
          suspended={lightboxOpen || helpOpen || exportOpen || migrateOpen || confirming}
          asset={activeAsset}
          batch={batchAssets}
          dirs={dirs}
          busy={busy}
          similarBusy={similarBusy}
          similarScore={activeAsset ? similarScores[activeAsset.assetId] : undefined}
          similarMin={similarSet ? similarMin : null}
          /* KI-14：档位住在 split button（工具条不再有模式开关）；菜单切档写偏好并立即执行 */
          similarMode={simMode}
          onSetSimilarMode={setSimMode}
          tagsVersion={tagsVersion}
          actions={{
            reject: (rej) => void applyReject(rej),
            addTags: (names) => void runAddTags(S.targetIds(sel), names),
            removeTag: (name) => {
              void runRemoveTag(name)
            },
            setColor: (c) => void runSetColor(c),
            moveTo: (d) => void runMove(S.targetIds(sel), d),
            findSimilar: (mode) => void findSimilar(mode),
          }}
          onPreview={openLightbox}
        />
      )}

      {/* E4 拖拽导出浮层（仅拖拽进行中出现；拖到目录/标签则不会 drop 到这里） */}
      {dragActive && (
        <div
          data-testid="export-dropzone"
          onDragOver={(e) => {
            e.preventDefault()
            e.dataTransfer.dropEffect = 'copy'
            if (!exportHover) setExportHover(true)
          }}
          onDragLeave={() => setExportHover(false)}
          onDrop={(e) => {
            e.preventDefault()
            setExportHover(false)
            const ids = takeDragPayload()
            setDragActive(false)
            if (ids.length > 0) {
              exportOverlay.openOverlay({ ids: ids.join(',') })
            }
          }}
          className={`fixed bottom-10 left-1/2 z-40 -translate-x-1/2 rounded-lg border-2 border-dashed px-8 py-4 text-sm backdrop-blur transition-colors ${
            exportHover
              ? 'border-accent bg-accent/20 text-ink'
              : 'border-accent/60 bg-panel/90 text-ink-dim'
          }`}
        >
          {t('⇩ 松开导出 {n} 张', { n: dragCount })}
          <span className="ml-2 text-xs text-ink-dim/70">
            {can(caps, 'moveAsset') ? t('（拖到左侧目录=移动 · 标签=打标）') : t('（标签=打标；目录移动需桌面版）')}
          </span>
        </div>
      )}

      {/* E2 Lightbox 全屏预览（大图 + 翻页 + 缩放；打星/拒选已移除 —— 编辑回检查器抽屉） */}
      {lightboxOpen && activeAsset && lightboxIndex >= 0 && (
        <Lightbox
          items={view}
          index={lightboxIndex}
          onIndex={onLightboxIndex}
          onClose={() => closeOverlay()}
          onHelp={() => openOverlay('help')}
          suspended={helpOpen}
        />
      )}

      {/* E2 导出对话框：单实例挂在 App（`?dialog=export&ids=…`，见 lib/modal.tsx useOverlay），
          本页不再自己渲染一份，避免两个实例各持一套状态 */}

      {/* DT7 迁入库对话框：URL 驱动（`?dialog=migrate-to-library&ids=…`）。跑完刷新列表 ——
          absPath / storeMode 已被后端改写，不刷新会继续显示旧路径与旧形态 */}
      <MigrateToLibraryDialog
        suspended={helpOpen}
        onFinished={() => setReloadKey((k) => k + 1)}
      />

      {/* P1-4 删除二次确认已并入 lib/confirm.tsx（useConfirm，全局单实例 Provider），
          本页不再渲染独立对话框组件 —— 避免两套确认浮层各持一套焦点/Esc 逻辑 */}

      {/* KI-10：卡片右键菜单 —— 「在文件夹中显示」/「复制路径」都由 <PathLink> 提供，
          翻遍全站只有这一个组件能渲染磁盘路径（口径见 references/path-reveal-matrix.md） */}
      {ctxMenu && (
        <div
          className="fixed z-[70] w-72 max-w-[92vw] rounded-md border border-line bg-panel p-2 shadow-xl"
          style={{
            left: Math.min(ctxMenu.x, Math.max(8, window.innerWidth - 300)),
            top: Math.min(ctxMenu.y, Math.max(8, window.innerHeight - 120)),
          }}
          data-testid="asset-context-menu"
          onContextMenu={(e) => e.preventDefault()}
        >
          <p className="mb-1 truncate text-xs text-ink" title={ctxMenu.asset.fileName}>
            {ctxMenu.asset.fileName}
          </p>
          <PathLink path={ctxMenu.asset.absPath} mode="reveal" />
        </div>
      )}

      {/* E2 快捷键速查 */}
      <ShortcutsHelp
        open={helpOpen}
        onClose={() => closeOverlay()}
        context={lightboxOpen ? 'lightbox' : 'library'}
      />
    </div>
  )
}

/** 其他页面共用的占位空状态（W3-W5 逐页填充） */
export function PagePlaceholder({
  icon,
  title,
  desc,
  hints,
}: {
  icon: string
  title: string
  desc: string
  hints: string[]
}) {
  const { t } = useI18n()
  return (
    <EmptyState
      icon={icon}
      title={title}
      desc={desc}
      hint={t('规划里程碑：{list}', { list: hints.join(' → ') })}
    />
  )
}


/**
 * EXIF2 元数据筛选条：画幅（横/竖/方）、相机型号、拍摄日期范围、排序键。
 * 全部走 0013 派生列（takenAt/cameraModel/aspectClass），不扫全表表达式。
 */
export function ExifFilterBar({
  filter,
  cameras,
  onChange,
  busy,
}: {
  filter: LibraryFilter
  cameras: { model: string; count: number }[]
  onChange: (f: LibraryFilter) => void
  busy: boolean
}) {
  const { t } = useI18n()
  const set = (patch: Partial<LibraryFilter>) => onChange({ ...filter, ...patch })
  const dateVal = (t?: number) => {
    if (t == null) return ''
    const d = new Date(t * 1000)
    const m = `${d.getMonth() + 1}`.padStart(2, '0')
    const day = `${d.getDate()}`.padStart(2, '0')
    return `${d.getFullYear()}-${m}-${day}`
  }
  const ts = (date: string, endOfDay: boolean) => {
    if (!date) return undefined
    const d = new Date(`${date}T00:00:00`)
    return Math.floor((endOfDay ? d.getTime() + 86_399_000 : d.getTime()) / 1000)
  }
  const aspectBtn = (v: 'landscape' | 'portrait' | 'square', label: string) => (
    <Button
      key={v}
      disabled={busy}
      active={filter.aspect === v}
      data-testid={`exif-aspect-${v}`}
      onClick={() => set({ aspect: filter.aspect === v ? undefined : v })}
    >
      {label}
    </Button>
  )
  return (
    <div
      className="flex shrink-0 flex-wrap items-center gap-2 border-b border-line bg-panel px-4 py-1.5 text-xs"
      data-testid="exif-filter-bar"
    >
      <span className="text-ink-dim">{t('元数据')}</span>
      {aspectBtn('landscape', t('横图'))}
      {aspectBtn('portrait', t('竖图'))}
      {aspectBtn('square', t('方图'))}
      <select
        data-testid="exif-camera-select"
        value={filter.cameraModel ?? ''}
        disabled={busy}
        onChange={(e) => set({ cameraModel: e.target.value || undefined })}
        className="rounded border border-line bg-panel-2 px-1.5 py-0.5 text-xs outline-none focus:border-accent"
      >
        <option value="">{t('相机型号')}</option>
        {cameras.map((c) => (
          <option key={c.model} value={c.model}>
            {c.model}（{c.count}）
          </option>
        ))}
      </select>
      <span className="text-ink-dim">{t('拍摄')}</span>
      <input
        type="date"
        data-testid="exif-date-from"
        value={dateVal(filter.takenFrom)}
        disabled={busy}
        onChange={(e) => set({ takenFrom: ts(e.target.value, false) })}
        className="rounded border border-line bg-panel-2 px-1.5 py-0.5 text-xs outline-none focus:border-accent"
      />
      <span className="text-ink-dim">~</span>
      <input
        type="date"
        data-testid="exif-date-to"
        value={dateVal(filter.takenTo)}
        disabled={busy}
        onChange={(e) => set({ takenTo: ts(e.target.value, true) })}
        className="rounded border border-line bg-panel-2 px-1.5 py-0.5 text-xs outline-none focus:border-accent"
      />
      <select
        data-testid="exif-sort-select"
        value={filter.sort ?? 'importedAt'}
        disabled={busy}
        onChange={(e) => set({ sort: e.target.value as 'importedAt' | 'takenAt' })}
        className="ml-auto rounded border border-line bg-panel-2 px-1.5 py-0.5 text-xs outline-none focus:border-accent"
      >
        <option value="importedAt">{t('排序：导入时间')}</option>
        <option value="takenAt">{t('排序：拍摄时间')}</option>
      </select>
    </div>
  )
}

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { EmptyState, ErrorBanner } from '../components/StateViews'
import { OcrChip } from '../components/OcrChip'
import { TagPicker } from '../components/TagPicker'
import { AssetCard } from '../components/AssetGrid'
import { InspectorDock, useInspectorDock } from '../components/InspectorDock'
import { useOverlay } from '../lib/modal'
import { Lightbox } from '../components/Lightbox'
import { ocrHint } from '../lib/ocrChip'
import { filterKey, describeBrowseFilters, normalizeTagQuery, TEXT_MIN_SCORE, STRICT_TEXT_MIN_SCORE, textMinLevel, textMinScoreOf } from '../lib/searchQuery'
import { useTasks } from '../lib/tasks'
import { useToast } from '../lib/toast'
import * as S from '../lib/selection'
import { hitOrder, orderByHits } from '../lib/searchResults'
import { stashPendingSet } from '../lib/tempSet'
import {
  ApiError,
  api,
  type AssetView,
  type BatchOpResult,
  type DirEntry,
  type ExportFilterPayload,
  type SearchFilterReq,
  type SearchHit,
  type SearchStatus,
} from '../lib/api'
import { stashExportFilterScope } from '../lib/export'
import { useUrlState } from '../lib/urlState'
import { SIMILAR_MODES, similarModeOption, useSimilarMode, type SimilarMode } from '../lib/similarMode'
import { useI18n } from '../i18n'
import { Button } from '../components/Button'
import { TextField } from '../components/TextField'
import { SelectionBar } from '../components/SelectionBar'
import { VerbActions } from '../components/VerbActions'

type Mode = 'text' | 'image'

/**
 * URL-STATE-1（卡 task / D-CHAIN-1）：搜索页可分享状态全部进 URL。
 * q=已执行的关键词（输入框只做未提交草稿）/ mode / asset=相似源图 / dir 目录（`path` 只读别名）/
 * ext 扩展名 / tags 标签 / k 条数 / simMin 文搜图相关性下限（严格/默认/宽松三档，见 threshold 控件）。
 * 刷新、深链、浏览器 back 都复现同一次搜索。
 */
const SEARCH_DEFAULTS = {
  q: '',
  mode: 'text' as Mode,
  asset: 0,
  dir: '',
  path: '',
  ext: '',
  tags: '',
  tag: '',
  k: 24,
  /** -1 = 未设置（URL 无此键）；0 = 宽松；0.1275 = 严格；其它数值 = 高级手动档 */
  simMin: -1,
}

function scoreBadge(score: number): string {
  return score >= 0.999 ? '1.000' : score.toFixed(3)
}

function ScoreColor(score: number): string {
  if (score >= 0.98) return 'bg-emerald-600/90'
  if (score >= 0.9) return 'bg-accent/90'
  if (score >= 0.8) return 'bg-amber-600/90'
  return 'bg-panel/90 text-ink-dim'
}

/**
 * 搜索页（W3 + 卡 task）：文搜图（中文语义）/ 以图搜图（上传或库内「找相似」）。
 *
 * 结果不再是「只能点开原图」的裸 <a>：
 * - 结果卡与图库同一张 AssetCard（单击选中 / cmd 加选 / shift 范围选 / 双击进 Lightbox）；
 * - 选择模型直接复用 lib/selection（与图库同一套语义：选择顺序 = 视图顺序）；
 * - KI-12：详情面与图库统一 —— 同一个 InspectorDock（useInspectorDock）挂在结果旁，
 *   数据源 = 当前选中集 + 结果集顺序；默认收起，选中后展开。禁止另写简化版 inspector；
 * - 搜索内可就地增删标签 / 色标 / 拒选 / 移动（同一套 api + 乐观更新）；**结果集不重算**，
 *   被改过的卡片带「已修改」角标，重跑搜索即还原最新数据；
 * - 「在图库中打开」保留：把结果作为临时集合交给图库继续工作（不再是唯一可编辑路径）；
 * - 原图保留为次要入口：卡片右键菜单「打开原图（新标签）」。
 * 未建向量索引时给引导（去设置页或一键增量嵌入）。
 */
export function Search() {
  const { t, locale } = useI18n()
  const navigate = useNavigate()
  const { startEmbed } = useTasks()
  const { toast } = useToast()

  // URL 是唯一真源（q/mode/asset/dir/ext/tags/k）；草稿（未提交的输入）留在本地
  const ui = useUrlState(SEARCH_DEFAULTS)
  // ?asset= 是「以图搜图」的深链：即使没带 mode，也按以图搜图呈现（与首屏自动重搜一致）
  const mode: Mode = ui.value.asset > 0 || ui.value.mode === 'image' ? 'image' : 'text'
  const k = ui.value.k > 0 ? ui.value.k : 24
  // `dir` 是目录键（历史链接与验收脚本都用它）；`path` 作为只读别名保留，兼容冻结契约里的写法
  const dirPrefix = ui.value.dir || ui.value.path
  const ext = ui.value.ext
  const tag = ui.value.tags || ui.value.tag

  const [draft, setDraft] = useState(ui.value.q)
  const query = draft
  const setQuery = setDraft

  // 外部改 URL（back/前进、其它页面「找相似」跳转）→ 草稿跟随
  useEffect(() => setDraft(ui.value.q), [ui.value.q])

  const setMode = useCallback((m: Mode) => ui.set({ mode: m }), [ui.set])
  const setK = useCallback((n: number) => ui.set({ k: n }), [ui.set])
  const setDirPrefix = useCallback((v: string) => ui.set({ dir: v }), [ui.set])
  const setExt = useCallback((v: string) => ui.set({ ext: v }), [ui.set])
  const setTag = useCallback((v: string) => ui.set({ tags: v }), [ui.set])

  // ---- KI-16 #4：文搜图相关性下限三档（严格/默认/宽松），URL `?simMin=` 单一真源 ----
  const simMinRaw = ui.value.simMin
  const simLevel = textMinLevel(simMinRaw)
  /** 本次文搜图请求要显式带的 minScore（默认档 = undefined → 后端按模型标定兜底） */
  const textMinReq = textMinScoreOf(simMinRaw)
  const setSimLevel = useCallback(
    (v: number) => ui.set({ simMin: v }),
    [ui.set],
  )

  /** 命中（含 score / OCR 证据）——卡片角标与提示用 */
  const [hits, setHits] = useState<SearchHit[]>([])
  /** 视图数据：命中按补全后的 AssetView（视图顺序 = 命中顺序） */
  const [results, setResults] = useState<AssetView[]>([])
  /** 选择模型（与图库同一套：lib/selection） */
  const [sel, setSel] = useState<S.SelState>(S.EMPTY)
  const [meta, setMeta] = useState<{
    model: string
    dims: number
    query: string
    /** 本次生效的相似度下限（后端按模型标定；文搜图 = 请求里显式传的值，缺省则 undefined） */
    minScore?: number
    minScoreCalibrated?: boolean
    /** task：'browse' = 纯过滤浏览（无 query，/api/library/assets 条件过滤，无相关性排序） */
    kind: 'text' | 'similar' | 'upload' | 'browse'
    /** browse 模式：/api/library/assets 回带的 total（与结果计数对账的唯一真值） */
    total?: number
  } | null>(null)
  /** 上传图缓存：过滤条件/条数变了要按同一张图重搜 */
  const [lastUpload, setLastUpload] = useState<File | null>(null)
  /** 条件已改但重搜还没回来（去抖窗口内） */
  const [stale, setStale] = useState(false)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<{ msg: string; code?: string } | null>(null)
  const [status, setStatus] = useState<SearchStatus | null>(null)
  const [dirs, setDirs] = useState<DirEntry[]>([])
  const [dropping, setDropping] = useState(false)
  const fileRef = useRef<HTMLInputElement>(null)

  // 浮层 / 批量动作
  const [lightboxOpen, setLightboxOpen] = useState(false)
  // UI-CARRIER-1：导出对话框改成 URL 驱动的单实例（挂在 App 里，`?dialog=export&ids=…`）
  const exportOverlay = useOverlay('dialog', 'export', ['ids'])
  const exportOpen = exportOverlay.open
  const [tagOpen, setTagOpen] = useState(false)
  const [tagNames, setTagNames] = useState('')
  /** 批量打标已选的标签（候选选中/回车加入；打标场景允许新建） */
  const [tagChips, setTagChips] = useState<string[]>([])
  const [batchBusy, setBatchBusy] = useState(false)
  const [menu, setMenu] = useState<{ assetId: number; x: number; y: number } | null>(null)
  /** KI-12：检查器抽屉（详情面与图库同一组件/同一 hook；autoOpenKey 区分会话记忆） */
  const [mutBusy, setMutBusy] = useState(false)
  const [tagsVersion, setTagsVersion] = useState(0)
  const [dirtyIds, setDirtyIds] = useState<ReadonlySet<number>>(new Set())

  const loadStatus = useCallback(() => {
    api.searchStatus().then(setStatus).catch(() => setStatus(null))
    api
      .dirs()
      .then((d) => setDirs(d.dirs))
      .catch(() => {})
  }, [])

  useEffect(() => {
    loadStatus()
  }, [loadStatus])

  // 图库变更（导入/嵌入完成）后刷新可用性
  useEffect(() => {
    const h = () => loadStatus()
    window.addEventListener('dam:library-changed', h)
    return () => window.removeEventListener('dam:library-changed', h)
  }, [loadStatus])

  const filter = useCallback(
    (): SearchFilterReq => ({
      tag: tag.trim() || undefined,
      dirPrefix: dirPrefix || undefined,
      ext: ext.trim() || undefined,
    }),
    [tag, dirPrefix, ext],
  )

  const clearFilters = useCallback(() => {
    // 过滤条件本身就是 URL 键（dir/ext/tags）→ 清空即从 URL 移除
    ui.set({ dir: '', path: '', ext: '', tags: '', tag: '' })
  }, [ui.set])

  /**
   * 导出 Dialog「当前筛选结果」范围的寄存体（卡 task）。
   * 只有 browse 模式（`/api/library/assets` 条件过滤）能无损映射成导出 filter；
   * 尺寸/日期维度是搜索端点独有、导出后端不认 → 有任一项生效就不寄存，避免静默丢条件。
   */
  const searchExportScope = useCallback((): Parameters<typeof stashExportFilterScope>[0] => {
    if (meta?.kind !== 'browse') return null
    const f = filter()
    if (f.minWidth || f.minHeight || f.dateFrom || f.dateTo) return null
    const q: ExportFilterPayload = {}
    if (f.tag) q.tag = f.tag
    if (f.dirPrefix) q.dirPrefix = f.dirPrefix
    if (f.ext) q.ext = f.ext
    return { label: t('搜索当前条件'), filter: q }
  }, [meta, filter, t, locale])

  /** 命中落地：先出卡片（占位视图），再回读真值补全；顺序恒为命中顺序 */
  const hitsSeq = useRef(0)
  const applyHits = useCallback(async (list: SearchHit[]) => {
    const seq = ++hitsSeq.current
    setHits(list)
    setSel(S.EMPTY)
    setDirtyIds(new Set()) // 新一次检索 = 新上下文，「已修改」角标随旧结果集一起作废
    setLightboxOpen(false)
    const ids = hitOrder(list)
    setResults(orderByHits(list, []))
    if (ids.length === 0) return
    try {
      const full = await api.assets({ ids })
      if (seq !== hitsSeq.current) return // 期间已发起新搜索：丢弃这次回读
      setResults(orderByHits(list, full.items))
    } catch {
      /* 回读失败：保留占位视图（卡片仍可选中/批量操作，只是缺元数据） */
    }
  }, [])

  /**
   * KI-14：找相似档位（与图库同一份实现 lib/similarMode；工具条不再有模式开关）。
   * 搜索页入口 = 「以图搜图」：上传区的档位选择器与检查器 split button 共用这一个值，
   * 文案与图库逐字一致（SIMILAR_MODES 是唯一真源）。
   */
  const { mode: similarMode, setMode: setSimilarMode } = useSimilarMode()
  /** 找相似请求进行中（只管这一个动作，不牵连文搜图） */
  const [simBusy, setSimBusy] = useState(false)

  const runText = useCallback(
    async (q: string) => {
      setBusy(true)
      setErr(null)
      try {
        // 相关性下限：三档控件（KI-16 #4）→ 默认档不传（后端按模型标定兜底），宽松 0 / 严格 0.1275 显式传
        const r = await api.searchText(q, k, filter(), textMinReq)
        await applyHits(r.hits)
        setMeta({
          model: r.model,
          dims: r.dims,
          query: q,
          minScore: textMinReq ?? TEXT_MIN_SCORE,
          minScoreCalibrated: false,
          kind: 'text',
        })
        // URL 记录「已执行的查询」：刷新/分享/back 都复现同一次搜索（纯筛选变更用 replace）
        ui.set({ q, asset: 0 })
      } catch (e) {
        setHits([])
        setResults([])
        setSel(S.EMPTY)
        setMeta(null)
        setErr({
          msg: e instanceof Error ? e.message : String(e),
          code: e instanceof ApiError ? e.code : undefined,
        })
      } finally {
        setBusy(false)
      }
    },
    [k, filter, ui.set, applyHits, textMinReq],
  )

  /**
   * 以图搜图（库内「找相似」）。KI-14：档位来自上传区旁选择器 / 检查器 split button（同一份 lib/similarMode）。
   * - semantic → /api/search/similar（mode 显式透传，语义塔 + 模型标定下限）；
   * - visual_precise → 判重塔 rerank + 0.90/0.98 分带（缺判重向量时显式提示，不装作「没有相似图」）。
   */
  const runSimilar = useCallback(
    async (assetId: number, override?: SimilarMode) => {
      const m: SimilarMode = override ?? similarMode
      setBusy(true)
      setSimBusy(true)
      setErr(null)
      try {
        if (m === 'visual_precise') {
          const r = await api.searchSimilarPrecise(assetId, k, filter())
          const nearPct = Math.round(r.nearThreshold * 100)
          const varPct = Math.round(r.variantThreshold * 100)
          if (!r.dedupIndexReady || r.queryMissingDedupVector) {
            setHits([])
            setResults([])
            setSel(S.EMPTY)
            setMeta(null)
            toast(
              'info',
              t('精准匹配暂不可用'),
              r.dedupIndexReady
                ? t('这张图还没有判重向量（先对库跑一次嵌入），暂无法判断是否近重复')
                : t('本库还没算过判重向量（先对库跑一次嵌入），精准匹配无从比较'),
            )
            return
          }
          await applyHits(r.hits)
          setMeta({
            model: r.model,
            dims: r.dims,
            query: t('#{id} · 找相似（精准匹配：同图变体 ≥{var}% / 近似重复 ≥{near}%）', { id: assetId, var: varPct, near: nearPct }),
            minScore: r.variantThreshold,
            minScoreCalibrated: true,
            kind: 'similar',
          })
          if (r.hits.length === 0) {
            toast('info', t('没有近重复或同图变体'), t('候选 {n} 张里没有相似度 ≥ {var}% 的图', { n: r.candidatePool, var: varPct }))
          } else if (r.missingDedupVectors > 0) {
            toast(
              'info',
              t('部分候选缺判重向量'),
              t('{n} 张候选没有判重向量，本次未参与比较（候选池 {pool} 张）', { n: r.missingDedupVectors, pool: r.candidatePool }),
            )
          }
          ui.set({ asset: assetId, q: '' })
          return
        }

        const r = await api.searchSimilar(assetId, k, filter(), undefined, 'semantic')
        await applyHits(r.hits)
        setMeta({
          model: r.model,
          dims: r.dims,
          query: t('#{id} 的相似图（语义相似）', { id: assetId }),
          minScore: r.minScore,
          minScoreCalibrated: r.minScoreCalibrated,
          kind: 'similar',
        })
        ui.set({ asset: assetId, q: '' })
      } catch (e) {
        setHits([])
        setResults([])
        setSel(S.EMPTY)
        setMeta(null)
        setErr({
          msg: e instanceof Error ? e.message : String(e),
          code: e instanceof ApiError ? e.code : undefined,
        })
      } finally {
        setBusy(false)
        setSimBusy(false)
      }
    },
    [k, filter, ui.set, applyHits, similarMode, toast],
  )

  const runUpload = useCallback(
    async (file: File) => {
      setBusy(true)
      setErr(null)
      setLastUpload(file)
      try {
        // 上传路径的 body 是字节流 → 条数/过滤只能走 query（AUDIT-B2 §1：此前被静默忽略）
        const r = await api.searchImage(file, { k, filter: filter() })
        await applyHits(r.hits)
        setMeta({
          model: r.model,
          dims: r.dims,
          query: t('上传图 {name}', { name: file.name }),
          minScore: r.minScore,
          minScoreCalibrated: r.minScoreCalibrated,
          kind: 'upload',
        })
      } catch (e) {
        setHits([])
        setResults([])
        setSel(S.EMPTY)
        setMeta(null)
        setErr({
          msg: e instanceof Error ? e.message : String(e),
          code: e instanceof ApiError ? e.code : undefined,
        })
      } finally {
        setBusy(false)
      }
    },
    [k, filter, applyHits],
  )

  // task（B07-1）：纯过滤通道——query 为空但填了任一过滤条件时，
  // 走与图库同一契约的 /api/library/assets（total 是计数对账的唯一真值）。
  // 结果区与向量搜索共用同一张卡片网格；browse 模式没有 score 角标（条件过滤不做语义打分）。
  const runBrowse = useCallback(async () => {
    setBusy(true)
    setErr(null)
    try {
      const r = await api.assets({
        tag: normalizeTagQuery(tag) || undefined,
        dirPrefix: dirPrefix || undefined,
        ext: ext.trim().toLowerCase() || undefined,
        limit: 1000,
        offset: 0,
      })
      ++hitsSeq.current // 作废在途的向量命中回读
      setHits([])
      setSel(S.EMPTY)
      setLightboxOpen(false)
      setResults(r.items)
      setMeta({
        model: '—',
        dims: 0,
        query: describeBrowseFilters(tag, dirPrefix, ext),
        kind: 'browse',
        total: r.total,
      })
      ui.set({ q: '', asset: 0 })
    } catch (e) {
      setHits([])
      setResults([])
      setSel(S.EMPTY)
      setMeta(null)
      setErr({
        msg: e instanceof Error ? e.message : String(e),
        code: e instanceof ApiError ? e.code : undefined,
      })
    } finally {
      setBusy(false)
    }
  }, [tag, dirPrefix, ext, ui.set])

  /** 有没有可独立成搜的过滤条件（B07-1：query 空 + 有条件 → 按钮不灰置） */
  const hasFilter = !!(tag.trim() || dirPrefix || ext.trim())

  // task（R40/KI-13）：标签输入统一用 TagPicker（候选逻辑只此一份，不再页面自写）。
  // 选中候选 → 写 URL 全值（q 空时去抖后走 browse，命中数 = 该值全量）；手填全值仍精确匹配。

  /** 当前结果对应的查询（用于条件变化后的自动重搜） */
  const rerunActive = useCallback(() => {
    if (ui.value.asset) return void runSimilar(ui.value.asset)
    if (lastUpload) return void runUpload(lastUpload)
    if (ui.value.q) return void runText(ui.value.q)
    if (hasFilter) void runBrowse() // 纯过滤结果同样跟随条件变化重查
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ui.value.asset, ui.value.q, lastUpload, tag, dirPrefix, ext, runSimilar, runUpload, runText, runBrowse])

  // 过滤条件 / 条数变化 → 去抖自动重搜（不再出现「条件已改、结果还是旧的」）
  // KI-16 #4：文搜图的相关性下限也是「条件」——换档自动重跑（与 ext/k 同一去抖通道）。
  // 只在当前重跑路径确实是文搜图时并入指纹，避免以图搜图/浏览模式多打一次请求。
  const isTextRerun = !!ui.value.q && !ui.value.asset && !lastUpload
  const fkey = filterKey(k, filter()) + (isTextRerun ? `|s${simMinRaw}` : '')
  const firstFkey = useRef(true)
  useEffect(() => {
    if (firstFkey.current) {
      firstFkey.current = false
      return
    }
    setStale(true)
    const t = setTimeout(() => {
      setStale(false)
      rerunActive()
    }, 450)
    return () => clearTimeout(t)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fkey])

  // 首屏：?asset=<id> 走找相似；?q=<query> 走文搜图；只带过滤条件走纯过滤浏览
  //（URL 是唯一真源 → 刷新/深链/back 同一套；B07-1 深链 ?tags=… 直接复现浏览结果）
  useEffect(() => {
    if (ui.value.asset) {
      void runSimilar(ui.value.asset)
      return
    }
    if (ui.value.q) {
      void runText(ui.value.q)
      return
    }
    if (ui.value.tags || ui.value.tag || ui.value.dir || ui.value.path || ui.value.ext) void runBrowse()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const indexNow = async () => {
    try {
      const r = await api.embedStart('pending')
      startEmbed(t('建向量索引'), t('增量嵌入 {n} 张（{model}）', { n: r.queued, model: r.model }))
    } catch (e) {
      setErr({ msg: e instanceof Error ? e.message : String(e), code: e instanceof ApiError ? e.code : undefined })
    }
  }

  // ---- 选择模型（与图库同一套语义） ----
  const order = useMemo(() => results.map((a) => a.assetId), [results])
  const pick = useCallback(
    (a: AssetView, mods: { shift: boolean; meta: boolean }) => {
      setMenu(null)
      setSel((s) => {
        if (mods.shift) return S.range(order, s, a.assetId)
        if (mods.meta) return S.toggle(s, order, a.assetId)
        return S.single(a.assetId)
      })
    },
    [order],
  )
  const openLightbox = useCallback((a: AssetView) => {
    setSel((s) => (s.ids.includes(a.assetId) ? { ...s, active: a.assetId } : S.single(a.assetId)))
    setLightboxOpen(true)
  }, [])

  /** 批量动作目标：有选择集用选择集，否则用游标（与图库 S.targetIds 一致） */
  const targets = S.targetIds(sel)
  const lightboxIndex = useMemo(() => results.findIndex((a) => a.assetId === sel.active), [results, sel.active])
  const hitById = useMemo(() => new Map(hits.map((h) => [h.assetId, h])), [hits])

  // ---- KI-12：检查器抽屉（与图库同一 hook / 同一组件，autoOpenKey: 'search'） ----
  /** 游标资产（单选详情） */
  const activeAsset = useMemo(() => results.find((a) => a.assetId === sel.active) ?? null, [results, sel.active])
  /** 批量编辑目标（结果集顺序里的选中资产） */
  const batchAssets = useMemo(
    () => sel.ids.map((id) => results.find((a) => a.assetId === id)).filter((a): a is AssetView => !!a),
    [sel.ids, results],
  )
  const inspHasSelection = !!activeAsset || batchAssets.length > 1
  const inspDock = useInspectorDock({ autoOpenKey: 'search', hasSelection: inspHasSelection })

  /** 就地修改过的资产 → 卡片「已修改」角标（结果集不重算的可见补偿） */
  const markDirty = useCallback((ids: number[]) => {
    setDirtyIds((prev) => {
      const next = new Set(prev)
      for (const id of ids) next.add(id)
      return next
    })
  }, [])

  /** 批量回执 → toast：部分失败逐条列出（不静默） */
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
      if (ids.length === 0) return
      const set = new Set(ids)
      setResults((prev) =>
        prev.map((a) => (set.has(a.assetId) ? { ...a, approvalStatus: rejected ? 'rejected' : 'approved' } : a)),
      )
      markDirty(ids)
      try {
        await api.reject(ids, rejected)
        toast('success', rejected ? t('已拒选 {n} 张', { n: ids.length }) : t('已取消拒选 {n} 张', { n: ids.length }))
      } catch (e) {
        toast('error', t('拒选失败'), e instanceof Error ? e.message : String(e))
      }
    },
    [sel, markDirty, toast],
  )

  /** 批量打标（复用图库的 /api/library/tag；结果里的勾选即目标集合）。
   *  task：chips（候选选中/回车加入）+ 输入框剩余文本（逗号/空格分隔）合并提交。 */
  const runAddTags = useCallback(async () => {
    const typed = tagNames
      .split(/[,，\s]+/)
      .map((s) => s.trim())
      .filter(Boolean)
    const names = [...new Set([...tagChips, ...typed])]
    const ids = S.targetIds(sel)
    if (names.length === 0 || ids.length === 0) return
    setBatchBusy(true)
    try {
      const r = await api.addTags(ids, names)
      reportBatch(t('已为 {n} 张加标签「{values}」', { n: r.okCount, values: names.join('、') }), r)
      if (r.okCount > 0) {
        markDirty(ids)
        setTagsVersion((v) => v + 1)
      }
      setTagOpen(false)
      setTagNames('')
      setTagChips([])
    } catch (e) {
      toast('error', t('加标签失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setBatchBusy(false)
    }
  }, [tagNames, tagChips, sel, reportBatch, markDirty, toast])

  // ---- KI-12：检查器动作（与图库同一套 api + 乐观更新；成功后标「已修改」，不重算结果集） ----
  const runInspectorAddTags = useCallback(
    async (names: string[]) => {
      const ids = S.targetIds(sel)
      if (ids.length === 0 || names.length === 0) return
      setMutBusy(true)
      try {
        const r = await api.addTags(ids, names)
        reportBatch(t('已为 {n} 张加标签「{values}」', { n: r.okCount, values: names.join('、') }), r)
        if (r.okCount > 0) {
          markDirty(ids)
          setTagsVersion((v) => v + 1)
        }
      } catch (e) {
        toast('error', t('加标签失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setMutBusy(false)
      }
    },
    [sel, reportBatch, markDirty, toast],
  )

  /** 解绑标签：Inspector 走「乐观移除 + 撤销窗口」，这里只负责落库 + 重取（与图库一致） */
  const runRemoveTag = useCallback(
    async (value: string) => {
      if (!activeAsset) return
      try {
        await api.removeTag(activeAsset.assetId, value)
        setTagsVersion((v) => v + 1)
        markDirty([activeAsset.assetId])
      } catch (e) {
        toast('error', t('解绑标签「{value}」失败', { value }), e instanceof Error ? e.message : String(e))
        setTagsVersion((v) => v + 1) // 服务端没删掉 → 重取把标签显示回来
      }
    },
    [activeAsset, markDirty, toast],
  )

  const runSetColor = useCallback(
    async (color: string | null) => {
      const ids = S.targetIds(sel)
      if (ids.length === 0) return
      setMutBusy(true)
      try {
        await api.setColor(ids, color)
        const set = new Set(ids)
        setResults((prev) => prev.map((a) => (set.has(a.assetId) ? { ...a, colorLabel: color } : a)))
        markDirty(ids)
        toast('success', color ? t('已给 {n} 张设色标（{color}）', { n: ids.length, color }) : t('已清除 {n} 张色标', { n: ids.length }))
      } catch (e) {
        toast('error', t('设置色标失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setMutBusy(false)
      }
    },
    [sel, markDirty, toast],
  )

  const runMove = useCallback(
    async (targetDir: string) => {
      const ids = S.targetIds(sel)
      if (ids.length === 0) return
      setMutBusy(true)
      try {
        const r = await api.moveAssets(ids, targetDir)
        reportBatch(t('已移动 {n} 张', { n: r.okCount }), r)
        const okIds = r.results.filter((x) => x.ok).map((x) => x.assetId)
        if (okIds.length > 0) {
          markDirty(okIds)
          // 移动改的是磁盘 absPath：按 ids 定点回读补真值（不是重算检索结果）
          api
            .assets({ ids: okIds })
            .then((res) => {
              const byId = new Map(res.items.map((a) => [a.assetId, a]))
              setResults((prev) => prev.map((a) => byId.get(a.assetId) ?? a))
            })
            .catch(() => {})
        }
      } catch (e) {
        toast('error', t('移动失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setMutBusy(false)
      }
    },
    [sel, reportBatch, markDirty, toast],
  )

  /** 把结果作为临时集合交给图库：在图库的工作台里继续处理（拖拽/导出/找相似等） */
  const openInGallery = useCallback(() => {
    const ids = targets
    if (ids.length === 0) {
      toast('warning', t('先选中要操作的结果'), t('单击选中 / Shift 范围选 / Cmd(Ctrl)+A 全选'))
      return
    }
    stashPendingSet({
      label: meta ? t('搜索「{q}」的 {n} 张结果', { q: meta.query, n: ids.length }) : t('搜索结果 {n} 张', { n: ids.length }),
      ids,
    })
    navigate('/gallery')
  }, [targets, meta, navigate, toast])

  // ---- 键位（与图库一致：cmd+A 全选 / Esc 清空 / ←→ 移动 / Enter 预览） ----
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement | null
      const typing =
        !!t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable)
      if (typing) return
      if (lightboxOpen || exportOpen || tagOpen) return
      const mod = e.metaKey || e.ctrlKey
      const key = e.key
      if (mod && (key === 'a' || key === 'A')) {
        e.preventDefault()
        setSel(S.all(order))
        return
      }
      if (key === 'Escape') {
        // KI-12：检查器抽屉开着时 Esc 归它（与图库同一优先级：浮层 → 抽屉 → 清空选择）
        if (inspDock.open) return
        setMenu(null)
        setSel(S.EMPTY)
        return
      }
      if (key === 'ArrowLeft' || key === 'ArrowRight') {
        if (order.length === 0) return
        e.preventDefault()
        setSel((s) => S.move(s, order, key === 'ArrowLeft' ? -1 : 1, e.shiftKey))
        return
      }
      if (key === 'Enter') {
        const a = results.find((x) => x.assetId === sel.active)
        if (a) {
          e.preventDefault()
          setLightboxOpen(true)
        }
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [order, results, sel.active, lightboxOpen, exportOpen, tagOpen, inspDock.open])

  const searchable = status?.searchable ?? false
  // 生效阈值：优先用本次响应回带的值（后端按模型解析后的真值），否则用 /status 的标定值
  const thresholdMin = meta?.minScore ?? status?.similarMinScore ?? 0
  const thresholdCalibrated = meta?.minScoreCalibrated ?? status?.similarMinScoreCalibrated ?? false
  const thresholdModel = meta?.model ?? status?.activeModel ?? '—'
  const noVectors = status !== null && !searchable
  const menuAsset = menu ? results.find((a) => a.assetId === menu.assetId) ?? null : null

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      {/* 搜索工具栏 */}
      <div className="shrink-0 border-b border-line bg-panel px-4 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <div className="flex overflow-hidden rounded-md border border-line">
            {(['text', 'image'] as Mode[]).map((m) => (
              <Button
                key={m}
                type="button"
                onClick={() => setMode(m)}
                variant="secondary"
                active={mode === m}
              >
                {m === 'text' ? t('文搜图') : t('以图搜图')}
              </Button>
            ))}
          </div>

          {mode === 'text' ? (
            <form
              className="flex min-w-0 flex-1 items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault()
                // B07-1：query 非空走向量搜索（行为不变）；query 空 + 有过滤条件 → 纯过滤浏览
                if (query.trim()) void runText(query.trim())
                else if (hasFilter) void runBrowse()
              }}
            >
              <TextField
                value={query}
                onDraftChange={setQuery}
                focusKey="search-query"
                placeholder={t('中文语义描述，如「雪山日出」「一只橘猫」「城市夜景」')}
                className="min-w-0 flex-1 rounded-md border border-line bg-panel-2 px-3 py-1.5 text-sm outline-none placeholder:text-ink-dim/70 focus:border-accent"
              />
              <Button
                type="submit"
                data-testid="search-submit"
                disabled={busy || (!query.trim() && !hasFilter)}
                title={query.trim() ? t('语义搜索') : t('按过滤条件浏览（无相关性排序）')}
                variant="primary"
                size="md"
              >
                {busy ? t('搜索中…') : t('搜索')}
              </Button>
            </form>
          ) : (
            <div className="flex min-w-0 flex-1 items-center gap-2">
              <Button
                type="button"
                onClick={() => fileRef.current?.click()}
                variant="secondary"
                size="md"
                className="border-dashed"
              >
                {t('选择一张图上传搜索')}
              </Button>
              <input
                ref={fileRef}
                type="file"
                accept="image/*"
                className="hidden"
                onChange={(e) => {
                  const f = e.target.files?.[0]
                  if (f) void runUpload(f)
                }}
              />
              <span className="text-xs text-ink-dim">{t('或把图片拖到这里 / 在图库检查器点「找相似」')}</span>
              {/*
                KI-14：找相似档位选择器 —— 搜索页不出现工具条模式开关，档位住在「以图搜图」入口旁
                （与检查器 split button 菜单同一份 SIMILAR_MODES 文案）。
                只作用于库内「找相似 / 右键找相似」；上传图搜索没有判重向量，永远是语义档。
              */}
              <span
                className="flex shrink-0 items-center gap-1 whitespace-nowrap pl-2"
                data-testid="search-similar-mode"
                role="group"
                aria-label={t('库内找相似档位（作用于「找相似」，不影响上传图搜索）')}
              >
                <span className="text-[11px] text-ink-dim">{t('找相似')}</span>
                {SIMILAR_MODES.map((opt) => (
                  <Button
                    key={opt.value}
                    type="button"
                    active={similarMode === opt.value}
                    data-testid={`search-similar-mode-${opt.value}`}
                    title={t(opt.title)}
                    onClick={() => setSimilarMode(opt.value)}
                    variant="secondary"
                    size="none"
                    className="px-2 py-0.5 text-[11px]"
                  >
                    {t(opt.label)}
                  </Button>
                ))}
                <span className="text-[11px] text-ink-dim/80" data-testid="search-similar-mode-note">
                  {t(similarModeOption(similarMode).desc)}
                </span>
              </span>
            </div>
          )}

          <label className="flex items-center gap-1 text-xs text-ink-dim">
            {t('条数')}
            <select
              value={k}
              onChange={(e) => setK(Number(e.target.value))}
              className="rounded border border-line bg-panel-2 px-1.5 py-1 text-xs"
            >
              {[12, 24, 48, 96].map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
        </div>

        {/* 过滤行（与图库同一套标量过滤：目录/扩展名/标签） */}
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
          <select
            value={dirPrefix}
            onChange={(e) => setDirPrefix(e.target.value)}
            className="max-w-[280px] rounded border border-line bg-panel-2 px-2 py-1 text-xs"
          >
            <option value="">{t('全部目录')}</option>
            {dirs.slice(0, 200).map((d) => (
              <option key={d.dir} value={d.dir}>
                {t('{dir}（{count}）', { dir: d.dir, count: d.count })}
              </option>
            ))}
          </select>
          <input
            value={ext}
            onChange={(e) => setExt(e.target.value)}
            placeholder={t('扩展名 jpg')}
            className="w-24 rounded border border-line bg-panel-2 px-2 py-1 text-xs outline-none focus:border-accent"
          />
          <div className="relative">
            <TagPicker
              testid="tag-filter-input"
              // 面板 testid 沿用历史 `tag-suggest*`（既有验收脚本/单测口径，不在本卡改契约）
              suggestTestid="tag"
              value={tag}
              onChange={setTag}
              // 选中候选 = 写 URL 全值 → 去抖后自动按全值重查；筛选场景不新建标签
              onPick={setTag}
              scenario="filter"
              placeholder={t('标签（输入可选候选）')}
              className="w-44 rounded border border-line bg-panel-2 px-2 py-1 text-xs outline-none focus:border-accent"
            />
          </div>
          {(dirPrefix || ext || tag) && (
            <Button
              type="button"
              onClick={clearFilters}
              variant="secondary"
            >
              {t('清空过滤')}
            </Button>
          )}
          <span className="ml-auto text-ink-dim">
            {status && (
              <>
                {t('模型')} <span className="text-ink">{status.activeModel || '—'}</span> · {t('维度')}{' '}
                {status.activeDims || '—'} · {t('向量')} <span className="text-ink">{status.activeRows}</span> {t('条')}
                {status.otherModelVectors && (
                  <span className="ml-2 text-amber-400">{t('库内还有其它模型的向量（embModel 隔离，不混算）')}</span>
                )}
              </>
            )}
          </span>
        </div>
        {/* 生效阈值（AUDIT-B2 §3 + KI-16 #4 人话化）：三档控件代替「放宽阈值」按钮，
            让用户看得到当前值、可微调、可感知方向；高级手动填数值收进折叠。 */}
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs" data-testid="threshold-bar">
          <span className="text-ink-dim">{t('生效阈值')}</span>
          {meta?.kind === 'browse' ? (
            <span className="rounded bg-panel-2 px-2 py-0.5 text-ink" data-testid="threshold-browse">
              {t('按条件浏览 · 无相似度阈值')}
            </span>
          ) : mode === 'text' ? (
            <span className="flex flex-wrap items-center gap-1.5" data-testid="simmin-control">
              <span className="text-ink-dim">{t('相关性下限')}</span>
              <span className="flex overflow-hidden rounded-md border border-line">
                {(
                  [
                    ['strict', t('严格')],
                    ['default', t('默认')],
                    ['loose', t('宽松')],
                  ] as const
                ).map(([lv, label]) => (
                  <Button
                    key={lv}
                    type="button"
                    data-testid={`simmin-${lv}`}
                    active={simLevel === lv}
                    onClick={() => setSimLevel(lv === 'default' ? -1 : lv === 'strict' ? STRICT_TEXT_MIN_SCORE : 0)}
                    variant="secondary"
                    size="none"
                    className="rounded-none border-0 px-2 py-0.5"
                    title={
                      lv === 'strict'
                        ? t('只看高相关命中（≥ {n}，默认 × 1.5）', { n: STRICT_TEXT_MIN_SCORE })
                        : lv === 'default'
                          ? t('模型标定值（≥ {n}），低于它的命中被隐藏', { n: TEXT_MIN_SCORE })
                          : t('不过滤相关性，全部命中可见（可能有低相关结果）')
                    }
                  >
                    {label}
                  </Button>
                ))}
              </span>
              <span className="text-ink-dim" data-testid="simmin-current">
                {simLevel === 'loose'
                  ? t('当前 0（不过滤）')
                  : simLevel === 'strict'
                    ? t('当前 {n}（默认 × 1.5）', { n: STRICT_TEXT_MIN_SCORE })
                    : simLevel === 'custom'
                      ? t('当前 {n}（手动）', { n: simMinRaw })
                      : t('当前 {n}（模型标定）', { n: TEXT_MIN_SCORE })}
              </span>
              <details className="text-ink-dim">
                <summary className="cursor-pointer select-none">{t('高级…')}</summary>
                <label className="ml-1 inline-flex items-center gap-1">
                  {t('手动填数值（0–1）')}
                  <input
                    data-testid="simmin-manual"
                    type="number"
                    min={0}
                    max={1}
                    step={0.005}
                    className="w-24 rounded border border-line bg-panel-2 px-1.5 py-0.5"
                    value={simLevel === 'custom' ? simMinRaw : ''}
                    placeholder={String(TEXT_MIN_SCORE)}
                    onChange={(e) => {
                      const v = Number(e.target.value)
                      // 空串/非法/越界不写 URL；合法值立即成为当前下限（重跑由 fkey 去抖通道接管）
                      if (e.target.value !== '' && Number.isFinite(v) && v >= 0 && v <= 1) setSimLevel(v)
                    }}
                  />
                </label>
              </details>
            </span>
          ) : thresholdCalibrated ? (
            <span className="rounded bg-panel-2 px-2 py-0.5 text-ink" data-testid="threshold-image">
              {t('相似度 ≥ {pct}%（模型 {model}，按模型标定）', { pct: Math.round(thresholdMin * 100), model: thresholdModel })}
            </span>
          ) : (
            <span className="rounded bg-panel-2 px-2 py-0.5 text-ink-dim" data-testid="threshold-image">
              {t('该模型未标定阈值：不过滤相似度（全部命中可见）')}
            </span>
          )}
          {stale && <span className="text-amber-400">{t('条件已修改 · 正在重搜…')}</span>}
        </div>
      </div>

      {/* 批量/选择动作条（KI-16 #5）：与图库同一份 SelectionBar，文案钉死「全选已加载 N 张 / 清空选择」，
          命中总数可知且大于已加载时由组件给出「仅作用于已加载的 N 张」提示行 */}
      {results.length > 0 && (
        <SelectionBar
          loaded={results.length}
          selected={sel.ids.length}
          total={meta?.kind === 'browse' ? meta.total : undefined}
          onSelectAll={() => setSel(S.all(order))}
          onClear={() => setSel(S.EMPTY)}
          actions={
            <>
              {/* KI35-P3：能力入口「送去处理」，与图库同一份实现（含「当前筛选结果」范围寄存） */}
              <VerbActions ids={sel.ids} filterScope={searchExportScope} />
              <Button type="button" data-testid="batch-tag-open" onClick={() => setTagOpen(true)} variant="secondary">
                {t('打标…（{n} 张）', { n: sel.ids.length })}
              </Button>
              <Button
                type="button"
                data-testid="batch-export"
                onClick={() => {
                  // 卡 task：导出 Dialog 的「当前筛选结果」范围需要页面寄存同一份查询条件。
                  // 只有 browse（/api/library/assets 条件过滤）能无损映射成导出 filter；
                  // 文搜图/相似图/上传图是**排序结果**不是筛选条件（没有可下发的条件），
                  // 尺寸/日期维度导出后端也不认 —— 这两种情况不寄存（Dialog 只给选择集范围），
                  // 宁可少一个选项，不做「界面说 M 张、实际导别的」的假读数。
                  stashExportFilterScope(searchExportScope())
                  exportOverlay.openOverlay({ ids: S.targetIds(sel).join(',') })
                }}
                variant="secondary"
              >
                {t('导出（{n} 张）', { n: sel.ids.length })}
              </Button>
              {/* KI-12：抽屉被手动关掉后本会话不再自动弹 —— 这里是搜索侧的唯一手动重开入口（与图库「详情」同义） */}
              <Button
                type="button"
                data-testid="search-inspector-toggle"
                active={inspDock.open}
                title={inspDock.open ? t('收起详情面板') : t('详情面板（元数据 / 标签 / 色标 / 移动）')}
                onClick={inspDock.toggle}
                variant="secondary"
              >
                {inspDock.open ? t('收起详情') : t('详情')}
              </Button>
            </>
          }
          trailing={
            <>
              <span className="text-ink-dim/70">{t('单击选中 · Cmd/Ctrl+单击加选 · Shift+单击范围选 · 双击预览 · 右键原图')}</span>
              <Button
                type="button"
                data-testid="batch-open-gallery"
                disabled={targets.length === 0}
                title={t('把当前结果作为临时集合交给图库继续处理')}
                onClick={openInGallery}
                variant="secondary"
                className="ml-auto"
              >
                {t('在图库中打开（{n} 张）', { n: targets.length })}
              </Button>
            </>
          }
        />
      )}

      {/* 未建索引引导 */}
      {noVectors && (
        <div className="mx-4 mt-4 rounded-md border border-amber-600/50 bg-panel p-4">
          <p className="text-sm font-semibold">{t('还没有当前模型的向量索引 —— 搜索需要先嵌入')}</p>
          <p className="mt-1 text-xs leading-5 text-ink-dim">
            {t('库内 {n} 张图待嵌入（模型 {model}）。', { n: status?.emb.none ?? 0, model: status?.activeModel || t('未配置') })}
            {t('向量按模型分表存放（禁混算），换 provider/模型后需要重跑一次嵌入。')}
          </p>
          <div className="mt-2 flex items-center gap-2">
            <Button
              type="button"
              onClick={() => void indexNow()}
              variant="primary"
            >
              {t('立即增量嵌入')}
            </Button>
            <Button
              type="button"
              onClick={() => navigate('/settings')}
              variant="secondary"
            >
              {t('去设置页配 provider')}
            </Button>
          </div>
        </div>
      )}

      {err && (
        <ErrorBanner
          title={err.code === 'no_vector_index' ? t('没有可用的向量索引') : t('搜索失败')}
          detail={err.msg}
          remedies={
            err.code === 'no_vector_index'
              ? [t('在设置页跑一次「增量嵌入」或「全库重嵌入」'), t('确认推理端点可达（设置页「测试连接」）')]
              : err.code === 'provider_not_configured'
                ? [t('设置页填 base_url + model（dam-ai / dashscope / 自定义）'), t('点「测试连接」确认端点返回向量')]
                : [t('确认后端在跑（/api/healthz）'), t('稍后重试；持续失败看设置页 provider 配置')]
          }
          onRetry={() => {
            if (ui.value.asset) void runSimilar(ui.value.asset)
            else if (query.trim()) void runText(query.trim())
            else if (hasFilter) void runBrowse()
            else loadStatus()
          }}
        />
      )}

      {/* 结果区 */}
      <div
        className="min-h-0 flex-1 overflow-y-auto px-4 py-3"
        onClick={() => setMenu(null)}
        onDragOver={(e) => {
          e.preventDefault()
          setDropping(true)
        }}
        onDragLeave={() => setDropping(false)}
        onDrop={(e) => {
          e.preventDefault()
          setDropping(false)
          const f = e.dataTransfer.files?.[0]
          if (f) {
            setMode('image')
            void runUpload(f)
          }
        }}
      >
        {busy && <p className="text-xs text-ink-dim">{t('推理中…（远程 embedding 端点往返）')}</p>}

        {!busy && results.length === 0 && !err && (
          <EmptyState
            icon="⌕"
            title={meta ? (meta.kind === 'browse' ? t('无匹配') : t('没有命中')) : t('开始搜索')}
            desc={
              meta ? (
                meta.kind === 'browse' ? (
                  // B07-2③：条件 0 命中要可见（「无匹配」+ 条件回显），不是静默空网格
                  t('{cond}下没有资产。标签要全值精确（可在输入框候选里选值），改宽条件或清空过滤再试。', { cond: meta.query ? t('当前条件（{q}）', { q: meta.query }) : t('当前条件') })
                ) : meta.kind === 'text' ? (
                  meta.minScore === 0 ? (
                    t('「{q}」在库里确实没有相关内容（已放宽到不过滤，模型 {model}）。', { q: meta.query, model: meta.model })
                  ) : (
                    t('「{q}」没有相似度 ≥ {pct}% 的结果（模型 {model}）：可能只是低于当前相关性下限（把「相关性下限」切到「宽松」看低相关结果），也可能库里确实没有。', { q: meta.query, pct: ((meta.minScore ?? TEXT_MIN_SCORE) * 100).toFixed(1), model: meta.model })
                  )
                ) : thresholdCalibrated ? (
                  t('没有相似度 ≥ {pct}% 的相似图（阈值按模型 {model} 标定，越低越宽松）；也可能是过滤条件（目录/扩展名/标签）把结果筛没了。', { pct: Math.round(thresholdMin * 100), model: meta.model })
                ) : (
                  t('「{q}」在当前库里没有结果（模型 {model}）。试试更宽泛的条件或换一张查询图。', { q: meta.query, model: meta.model })
                )
              ) : (
                t('文搜图：输入中文描述（雪山、咖啡馆、情侣、夜景…）；以图搜图：上传一张图，或选中图库图片后点「找相似」。')
              )
            }
            actions={
              meta ? (
                <Button
                  type="button"
                  onClick={clearFilters}
                  variant="secondary"
                >
                  {t('清空过滤重搜')}
                </Button>
              ) : undefined
            }
            hint={dropping ? t('松开鼠标用这张图搜索') : undefined}
          />
        )}

        {results.length > 0 && (
          <>
            {meta && (
              <p className="mb-2 text-xs text-ink-dim" data-testid="search-source-badge">
                {meta.kind === 'browse' ? (
                  <>
                    {t('按条件浏览')}{meta.query ? t('（{q}）', { q: meta.query }) : ''} · {t('共')}{' '}
                    <span className="text-ink" data-testid="browse-total">
                      {meta.total ?? results.length}
                    </span>{' '}
                    {t('张 · 条件过滤无相关性排序')}
                  </>
                ) : (
                  <>
                    {t('「{q}」命中', { q: meta.query })} <span className="text-ink">{results.length}</span> {t('条 · 模型 {model}（', { model: meta.model })}
                    {t('{dims} 维）· 按余弦相似度降序', { dims: meta.dims })}
                  </>
                )}
              </p>
            )}
            <div
              className="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-2"
              data-testid="search-results"
            >
              {results.map((a, i) => {
                const hit = hitById.get(a.assetId)
                return (
                  <div
                    key={a.assetId}
                    className="relative"
                    data-testid={`search-result-${a.assetId}`}
                    onContextMenu={(e) => {
                      e.preventDefault()
                      setMenu({ assetId: a.assetId, x: e.clientX, y: e.clientY })
                    }}
                  >
                    <AssetCard
                      asset={a}
                      isSel={S.isOn(sel, a.assetId)}
                      isActive={sel.active === a.assetId}
                      selectedIds={sel.ids}
                      onPick={pick}
                      onOpen={openLightbox}
                      dirty={dirtyIds.has(a.assetId)}
                    />
                    {hit && (
                      <span
                        className={`pointer-events-none absolute top-1 right-1 rounded px-1.5 py-0.5 text-[10px] font-semibold text-white ${ScoreColor(
                          hit.score,
                        )}`}
                        title={t('命中排序 #{rank} · score {score}\n{ocr}', { rank: i + 1, score: scoreBadge(hit.score), ocr: ocrHint(hit) ?? '' })}
                      >
                        #{i + 1} · {scoreBadge(hit.score)}
                      </span>
                    )}
                    {hit && <OcrChip hit={hit} />}
                  </div>
                )
              })}
            </div>
          </>
        )}
      </div>

      {/* 右键菜单：原图是次要入口（主入口是卡片选中 + 批量动作） */}
      {menu && menuAsset && (
        <div
          data-testid="search-context-menu"
          className="fixed z-50 w-40 overflow-hidden rounded-md border border-line bg-panel text-xs shadow-lg"
          style={{ left: menu.x, top: menu.y }}
          onClick={(e) => e.stopPropagation()}
        >
          <a
            href={api.originalUrl(menuAsset.assetId)}
            target="_blank"
            rel="noreferrer"
            data-testid="context-open-original"
            className="block px-3 py-1.5 hover:bg-panel-2"
            onClick={() => setMenu(null)}
          >
            {t('打开原图（新标签）')}
          </a>
          <button
            type="button"
            data-testid="context-select"
            className="block w-full px-3 py-1.5 text-left hover:bg-panel-2"
            onClick={() => {
              setSel(S.single(menuAsset.assetId))
              setMenu(null)
            }}
          >
            {t('仅选中这张')}
          </button>
          <button
            type="button"
            data-testid="context-find-similar"
            className="block w-full px-3 py-1.5 text-left hover:bg-panel-2"
            onClick={() => {
              setMenu(null)
              void runSimilar(menuAsset.assetId)
            }}
          >
            {t('找相似')}
          </button>
        </div>
      )}

      {/* 批量打标对话框（复用 /api/library/tag） */}
      {tagOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50" data-testid="batch-tag-dialog">
          <form
            className="w-80 rounded-lg border border-line bg-panel p-4"
            onSubmit={(e) => {
              e.preventDefault()
              void runAddTags()
            }}
          >
            <p className="text-sm font-semibold">{t('给 {n} 张结果打标签', { n: targets.length })}</p>
            <p className="mt-1 text-xs text-ink-dim">
              {t('输入可选候选；回车加入已选，或直接输入新值（逗号 / 空格分隔可多个）')}
            </p>
            {tagChips.length > 0 && (
              <div className="mt-2 flex flex-wrap gap-1.5" data-testid="batch-tag-chips">
                {tagChips.map((v) => (
                  <span
                    key={v}
                    className="flex items-center gap-1 rounded-full border border-line bg-panel-2 px-2 py-0.5 text-xs"
                  >
                    {v}
                    <Button
                      variant="icon"
                      size="none"
                      data-testid={`batch-tag-chip-remove-${v}`}
                      title={t('移除「{value}」', { value: v })}
                      className="h-4 w-4 shrink-0 rounded-full border-0 bg-transparent text-ink hover:bg-line hover:text-red-400"
                      onClick={() => setTagChips((prev) => prev.filter((x) => x !== v))}
                    >
                      ×
                    </Button>
                  </span>
                ))}
              </div>
            )}
            <TagPicker
              testid="batch-tag-input"
              value={tagNames}
              onChange={setTagNames}
              // 选中候选 = 直接加入已选（打标场景允许多选成 chips）
              onPick={(v) => setTagChips((prev) => (prev.includes(v) ? prev : [...prev, v]))}
              // 无匹配回车 = 新建（输入里若有逗号/空格，按多个新标签一起加入；逐项归一化）
              onCommit={(raw) => {
                const vs = raw
                  .split(/[,，\s]+/)
                  .map((s) => normalizeTagQuery(s))
                  .filter(Boolean)
                setTagChips((prev) => [...new Set([...prev, ...vs])])
                setTagNames('')
              }}
              scenario="tag"
              placeholder={t('如：雪山, 日出')}
              className="mt-2 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
              panelClassName="absolute top-full left-0 z-40 mt-1 max-h-56 w-full overflow-y-auto rounded-md border border-line bg-panel p-1 text-xs shadow-lg"
            />
            <div className="mt-3 flex justify-end gap-2">
              <Button
                type="button"
                onClick={() => {
                  setTagOpen(false)
                  setTagNames('')
                  setTagChips([])
                }}
                variant="secondary"
              >
                        {t('取消')}
                      </Button>
              <Button
                type="submit"
                data-testid="batch-tag-submit"
                disabled={batchBusy || (!tagNames.trim() && tagChips.length === 0)}
                variant="primary"
              >
                {batchBusy ? t('打标中…') : t('打标')}
              </Button>
            </div>
          </form>
        </div>
      )}

      {/* KI-12：检查器抽屉 —— 与图库同一 InspectorDock（同一 hook，字段/动作完全一致，无简化版）。
          数据源 = 当前选中集 + 结果集顺序；默认收起，选中后展开（本会话手动关过就不再自动弹）。 */}
      <InspectorDock
        open={inspDock.open}
        onClose={inspDock.close}
        /* 上层浮层（Lightbox / 批量打标 / 导出）开着时挂起抽屉 Esc，一次只关一层 */
        suspended={lightboxOpen || tagOpen || exportOpen}
        asset={activeAsset}
        batch={batchAssets}
        dirs={dirs}
        busy={mutBusy}
        similarBusy={simBusy}
        /* KI-14：档位住在 split button（搜索页不出现工具条模式开关，档位与上传区选择器同步） */
        similarMode={similarMode}
        onSetSimilarMode={setSimilarMode}
        tagsVersion={tagsVersion}
        onPreview={openLightbox}
        actions={{
          reject: (rej) => void applyReject(rej),
          addTags: (names) => void runInspectorAddTags(names),
          removeTag: (name) => void runRemoveTag(name),
          setColor: (c) => void runSetColor(c),
          moveTo: (d) => void runMove(d),
          findSimilar: (mode) => {
            if (activeAsset) void runSimilar(activeAsset.assetId, mode)
            else toast('warning', t('先选中一张图片'), t('「找相似」以当前选中图为查询'))
          },
        }}
      />

      {/* Lightbox 全屏预览（大图 + 翻页 + 缩放；编辑一律回检查器抽屉） */}
      {lightboxOpen && lightboxIndex >= 0 && (
        <Lightbox
          items={results}
          index={lightboxIndex}
          onIndex={(i) => {
            const a = results[i]
            if (!a) return
            setSel((s) => (s.ids.length > 1 && s.ids.includes(a.assetId) ? { ...s, active: a.assetId } : S.single(a.assetId)))
          }}
          onClose={() => setLightboxOpen(false)}
          onHelp={() => {}}
          suspended={exportOpen || tagOpen}
        />
      )}

    </div>
  )
}

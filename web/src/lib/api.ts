/**
 * W2 后端 API 客户端（/api/*，vite dev 反代 / axum 同源直连）。
 * 全部真实链路：导入（扫描/启动/进度/取消/续跑/历史）+ 图库读模型 + 媒体 URL。
 */
import { searchImageQuery } from './searchQuery'
import { getGlobalLocale, tGlobal } from '../i18n'
import type { PackText } from '../i18n/pickI18n'

export interface AssetView {
  assetId: number
  mediaType: string
  absPath: string
  storeMode: string
  missing: number
  fileName: string
  fileExt: string
  fileSz: number
  mtime: number
  fileHash: string | null
  titleCh: string | null
  titleEn: string | null
  description: string | null
  source: string
  parentAssetId: number | null
  searchable: number
  deleted: number
  approvalStatus: string
  starRating: number
  colorLabel: string | null
  cloudAssetId: string | null
  importedAt: number
  embStatus: string
  embModel: string | null
  dedupStatus: string
  dedupModel: string | null
  thumbPath: string | null
  width: number | null
  height: number | null
  pHash: string | null
  // ---- EXIF2 元数据（真值在 exif json；null = 未解析/无该元数据） ----
  takenAt: number | null
  cameraMake: string | null
  cameraModel: string | null
  lensModel: string | null
  iso: number | null
  focalLenMm: number | null
  aperture: number | null
  shutterText: string | null
  orientation: number | null
  aspectRatio: number | null
  aspectClass: 'landscape' | 'portrait' | 'square' | null
  gpsLat: number | null
  gpsLon: number | null
  gpsSource: string | null
  exif: string | null
}

export interface TagRecord {
  id: number
  assetId: number
  /** 标签值（TAG-VALUE：tag 只有值，没有维度名） */
  value: string
  /** 来源：auto(模板打标) / manual(人工) / path(目录) / custom(自定义任务模型) */
  origin: string
  model: string | null
  /** 仅 origin='custom'：产出该标签的自定义任务 */
  taskId?: number
  score: number
  createdAt: number
}

/** 打标任务结果行（tag_result）：name=维度名（任务名快照），value=模型输出原文 */
export interface TagResultRecord {
  id: number
  assetId: number
  taskId: number | null
  name: string
  value: string
  model: string | null
  score: number
  createdAt: number
  updatedAt: number
}

export interface LibraryStats {
  assets: number
  /** 上次现场核查得到的失联数 —— 不是实时值，必须连 missingStale/missingCheckedAt 一起看 */
  missing: number
  tagged: number
  totalBytes: number
  byMediaType: { mediaType: string; count: number }[]
  /** 上次全库失联核查时间（unix 秒）；null = 从未核查 */
  missingCheckedAt: number | null
  /** 统计已过期（从未核查 / 超过 missingStaleSecs）→ UI 提示「待核查」+ 重新核查 */
  missingStale: boolean
  /** 上次核查中「存在但读不了」的数量（权限/IO），不计入 missing */
  missingUnreadable: number
  /** D3 无法预览：解码失败（无尺寸）的资产数 —— 与 assets?broken=1 同一口径 */
  broken: number
  /** 新鲜度阈值（秒），与后端同一口径 */
  missingStaleSecs: number
  /** GH-5：判重塔（DINOv3）可用 —— 权重两件套在模型根且构建编入本地推理；false = 判重不可用 */
  dedupAvailable: boolean
  /** false 时的自备指引（「判重不可用，需自备模型（Meta 许可）」+ 模型根路径）；true 时缺省 */
  dedupUnavailableReason?: string
}


export interface DirEntry {
  dir: string
  count: number
}

export interface TagCount {
  /** 标签值（侧栏标签云） */
  value: string
  count: number
}

/**
 * 候选行（`/api/library/tags?prefix=|key=|offset=`，t_d4895738）：
 * `key` = 值里第一个 `-` 前的段（key-val 形态，如 `尺寸-滚筒_10KG` → `尺寸`）；裸值 → null。
 * 分组只是展示层，选中/筛选仍用完整 `value`。
 */
export interface TagCandidateItem extends TagCount {
  key: string | null
}

/** `/api/library/tags?prefix=` 响应（t_4f145794 候选提示；不带 prefix/key/offset 时仍是 TagCount[]） */
export interface TagCandidatesResp {
  items: TagCandidateItem[]
  /** 命中的标签值总数（未截断；items 被 limit 截断时 hasMore=true） */
  total: number
  hasMore: boolean
}

/** 标签总览左栏一行（`/api/library/tag-groups`，t_cafb2e45）：key + 该组值数。
 *  `key === ''` = 无前缀裸值组（值里没有 `-`，或 `-` 在首位）。 */
export interface TagGroupItem {
  key: string
  values: number
}

export interface TagGroupsResp {
  items: TagGroupItem[]
  /** 在库标签值总数（各组之和） */
  total: number
}

/** 合并/改名回执（`/tags/merge` `/tags/rename` `/tags/merge-preview`） */
export interface TagMergeOutcome {
  /** 受影响资产数（去重、仅在库） */
  affected: number
  /** 会消失的源值 */
  removedValues: string[]
  /** 保留值 */
  keptValue: string
}

/** 删除回执（`/tags/delete`，只允许在库计数为 0 的值） */
export interface TagDeleteOutcome {
  deleted: string[]
  removedRows: number
}

export interface ImportFailure {
  /** 文件名（不含目录） */
  fileName: string
  /** 绝对路径（「重试失败项」回传后端） */
  path: string
  /** 失败原因原文 */
  error: string
}

export interface ImportProgress {
  batchId: number
  status: string
  done: number
  total: number
  imported: number
  duplicates: number
  failed: number
  lastError: string | null
  currentPath: string | null
  /** 累计失败明细（上限 200；failed 计数是全量） */
  failedItems?: ImportFailure[]
  /** E5 导入即图库：尚未处理到的文件名（按处理序），渲染为网格半透明占位卡 */
  pendingNames: string[]
}

export interface BatchOutcome {
  batchId: number
  status: string
  total: number
  imported: number
  duplicates: number
  unchanged: number
  updated: number
  failed: number
  /** 失败明细（任务中心「失败明细 / 重试失败项」数据源） */
  failedItems?: ImportFailure[]
}

export interface ImportStatus {
  running: boolean
  progress: ImportProgress | null
  outcome: BatchOutcome | null
}

export interface ImportBatchRecord {
  batchId: number
  rootPath: string
  storeMode: string
  status: string
  totalFiles: number
  doneFiles: number
  error: string | null
  startedAt: number
  endedAt: number | null
}

export interface ScanResult {
  imageFiles: number
  totalBytes: number
  preview: string[]
}

// ---------- W3：搜索 / 推理 provider / 向量索引 ----------

/** 单条搜索命中（engine-search::SearchHit）。score = 余弦相似度。 */
export interface SearchHit {
  assetId: number
  resourceId: number
  score: number
  fileName: string
  titleCh: string | null
  thumbPath: string | null
  width: number | null
  height: number | null
  /** OCR-MATCH：该命中含 assets.ocr_text 字面证据（非纯语义命中） */
  ocrMatched?: boolean
  /** OCR-MATCH：命中的原文片段（匹配行，截断） */
  ocrSnippet?: string | null
}

/** 搜索标量过滤（HTTP 形态；camelCase）。 */
export interface SearchFilterReq {
  tag?: string
  dirPrefix?: string
  ext?: string
  minWidth?: number
  minHeight?: number
  dateFrom?: number
  dateTo?: number
}

export interface SearchResult {
  model: string
  dims: number
  k: number
  count: number
  hits: SearchHit[]
  /** 以图搜图：本次实际生效的相似度下限（后端按模型标定；文搜图不返回） */
  minScore?: number
  /** 该模型的下限是否已人工标定（false = 未标定模型，后端不过滤，UI 别显示百分比） */
  minScoreCalibrated?: boolean
}

/** 找相似模式（KI-5）：语义相似（默认，行为不变）/ 精准匹配（同图变体、近似重复） */
export type SimilarMode = 'semantic' | 'visual_precise'

/** 精准模式命中行：`score` 是判重塔 cos（不是语义分），`band` 是分带。 */
export interface PreciseHit {
  assetId: number
  resourceId: number
  /** 判重塔 cos（本次排序分） */
  score: number
  /** near_duplicate（≥0.98 压缩/重存）/ variant（0.90~0.98 裁剪/换色/加字） */
  band: 'near_duplicate' | 'variant'
  fileName: string
  titleCh: string | null
  thumbPath: string | null
  width: number | null
  height: number | null
  /** 同一候选的语义塔 cos（诊断：它凭什么叫候选） */
  semanticScore: number
}

/**
 * 精准匹配结果（KI-5）。除 hits 外必须读这三项，否则会「静默变少」：
 * - `dedupIndexReady=false` → 全库还没算过判重向量（先跑嵌入）；
 * - `queryMissingDedupVector` → 查询图自己没有判重向量（无从比较）；
 * - `missingDedupVectors>0` → 候选里这么多张缺向量，本次没参与比较。
 */
export interface PreciseResult extends Omit<SearchResult, 'hits' | 'minScore' | 'minScoreCalibrated'> {
  mode: 'visual_precise'
  hits: PreciseHit[]
  /** 语义塔扩池后的候选数（不含查询图自身） */
  candidatePool: number
  missingDedupVectors: number
  queryMissingDedupVector: boolean
  dedupIndexReady: boolean
  nearThreshold: number
  variantThreshold: number
  selfExcluded: boolean
}

/** 向量表（每个 embModel 一张，禁混算）。 */
export interface VectorModelInfo {
  model: string
  dims: number
  table: string
  rows: number
  active: boolean
}

export interface ScopeStats {
  done: number
  failed: number
  pending: number
  remaining: number
  vectors: number
}

export interface EmbedRunReportView {
  scope: string
  model: string
  processed: number
  done: number
  failed: number
  retry: number
  skipped: number
  cancelled: boolean
  errors: string[]
  jobDone: number
  jobFailed: number
  jobPending: number
}

export interface EmbedProgressView {
  jobId: number
  assetId: number
  scope: string
  model: string
  status: string
  index: number
  total: number
  doneCount: number
  failedCount: number
  error?: string
}

/** GET /api/embed/status */
export interface EmbedStatus {
  running: boolean
  scope: string | null
  progress: EmbedProgressView | null
  report: EmbedRunReportView | null
  lastError: string | null
  activeModel: string
  stats: { semantic: ScopeStats; dedup: ScopeStats }
  models: VectorModelInfo[]
}

/** GET /api/search/status */
export interface SearchStatus {
  activeModel: string
  activeDims: number
  activeRows: number
  searchable: boolean
  models: VectorModelInfo[]
  emb: { none: number; pending: number; done: number; failed: number }
  semantic: ScopeStats
  otherModelVectors: boolean
  /** 以图搜图相关性下限（当前模型标定值；未标定模型 = 0） */
  similarMinScore: number
  similarMinScoreCalibrated: boolean
}

/** registry 里的 embedding spec（设置页下拉）。 */
export interface EmbSpec {
  name: string
  kind: string
  dims: number
  modalities: string[]
  provider: string
  preprocess: unknown
}

export interface RemoteEmbedDto {
  base_url: string
  api_key: string
  model: string
  dims: number
  timeout_secs: number
  proxy: string | null
}

/** W4：打标 provider 设置（core_config::TagSettings）。 */
export interface TagSettingsDto {
  enabled: boolean
  /** D9 预设 id：damai | dashscope | zhipu | ollama | custom */
  provider: string
  base_url: string
  api_key: string
  model: string
  prompt_template: string
  max_tokens: number
  timeout_ms: number
  proxy: string | null
}

/** D9/R7 预设表条目（core_config::TagProviderPreset）。 */
export interface TagProviderPresetDto {
  id: string
  label: string
  base_url: string
  models: string[]
  scan: boolean
}

export interface InferenceSettingsDto {
  active_embedder: string
  model_base_url: string
  auto_download: boolean
  embedding: RemoteEmbedDto
  tag: TagSettingsDto
}

export interface InferenceResponse {
  settings: InferenceSettingsDto
  /**
   * 活跃 embedder 候选 = registry 里 modalities 含 text 的 embedding spec（文搜图前提）。
   * 去重塔等仅图像塔不在其中、也不单列（KI-6：设置界面不展示去重模型信息）。
   */
  embedders: EmbSpec[]
  capabilities: { localInfer: boolean; remoteProvider: boolean; note: string }
}

// ---------- W4：打标（模板队列 + 自定义任务） ----------

export interface TagProgressView {
  jobId: number
  assetId: number
  status: string
  index: number
  total: number
  doneCount: number
  failedCount: number
  error?: string
}

export interface TagRunReportView {
  processed: number
  done: number
  failed: number
  retry: number
  skipped: number
  /** 输出被 max_tokens 截断、靠结构补齐救回的张数 */
  repaired?: number
  cancelled: boolean
  errors: string[]
  jobDone: number
  jobFailed: number
  jobPending: number
}

export interface TagQueueStatsView {
  done: number
  failed: number
  pending: number
  untaggedAssets: number
}

/** 与 core_config::TagSettings::DEFAULT_MAX_TOKENS 对齐（权威值见
 *  /api/tag/status.provider.defaultMaxTokens / maxTokensLabel）。 */
export const DEFAULT_TAG_MAX_TOKENS = 2048

export interface TagFailedItemView {
  jobId: number
  assetId: number
  attempts: number
  lastError: string
  /** truncated = max_tokens 截断；other = 网络/上游/非法输出 */
  errorClass?: 'truncated' | 'other' | string
}

export interface CustomTagFailedItemView extends TagFailedItemView {
  taskId: number
  taskName: string
}

// ---- OCR-E 文字识别队列 ----

/** GET /api/ocr/status（stats/progress/report 与 tag 同形；noOcrAssets = 待识别数） */
export interface OcrQueueStatsView {
  done: number
  failed: number
  pending: number
  noOcrAssets: number
}

export interface ModelFileStatus {
  name: string
  urlPath: string
  present: boolean
  bytes: number
  expectedBytes: number
  expectedSha256: string
}

/** 单个 manifest 的落盘状态（GET /api/models） */
export interface ModelEntry {
  name: string
  /** true = 未随安装包内置、需要用户下载（GH-9：4 个可选增强 + lama，约 504MB） */
  downloadable: boolean
  ready: boolean
  presentBytes: number
  totalBytes: number
  /** 许可（展示口径，后端 model_info 表） */
  license?: string
  /** 来源描述（上游仓/导出方） */
  source?: string
  /** 缺件的绝对路径（引擎实际读取的位置，可直接照着手放） */
  missingPaths: string[]
  files: ModelFileStatus[]
}

export interface ModelsView {
  root: string
  baseUrl: string | null
  /** 生效分发源列表（default = HF 主源 → COS 镜像；override = 用户显式指定，单项） */
  sources?: string[] | null
  /** default = 内置镜像组自动 failover；override = settings.model_base_url / 请求显式指定 */
  sourceMode?: 'default' | 'override'
  downloadable: string[]
  /** 聚合（GH-9）：未就绪的可选模型个数 / 缺件字节 / 可选模型总体积 */
  missingCount?: number
  missingBytes?: number
  optionalBytes?: number
  models: ModelEntry[]
  /** web 构建（local-infer off）时后端给的口径说明 */
  note?: string
}

/** 下载进度快照（GET /api/models/download/status） */
export interface ModelDownloadStatus {
  running: boolean
  model: string | null
  status: 'downloading' | 'done' | 'failed' | null
  doneBytes: number
  totalBytes: number
  completedFiles: number
  totalFiles: number
  error: string | null
  outcome: unknown
  startedAt: number | null
  finishedAt: number | null
}

export interface OcrStatusView {
  running: boolean
  stats: OcrQueueStatsView
  progress: {
    jobId: number
    assetId: number
    status: string
    index: number
    total: number
    doneCount: number
    failedCount: number
    error?: string
  } | null
  report: {
    processed: number
    done: number
    failed: number
    retry: number
    skipped: number
    cancelled: boolean
    errors: string[]
    jobDone: number
    jobFailed: number
    jobPending: number
  } | null
  error: string | null
  /** 三件模型（det/rec/dict）是否在盘；false 时识别必失败，UI 要常驻提示 */
  modelsReady?: boolean
  /** 未就绪时缺失的绝对路径清单（后端给出，用户不必猜目录结构） */
  missingModelFiles?: string[]
}

/** GET /api/tag/tasks 条目（core_types::CustomTagTask）。 */
export interface CustomTagTaskDto {
  id: number
  name: string
  prompt: string
  enabled: boolean
  createdAt: number
  updatedAt: number
  jobPending: number
  jobDone: number
  jobFailed: number
  lastRunAt?: number
}

export interface TagQueueView {
  running: boolean
  progress: TagProgressView | null
  report: TagRunReportView | null
  lastError: string | null
}

/** GET /api/tag/status */
export interface TagStatusView {
  template: TagQueueView
  custom: TagQueueView & { pendingJobs: number }
  /** E5 确认流：待确认提案统计 */
  review: { assets: number; rows: number }
  stats: TagQueueStatsView
  provider: {
    enabled: boolean
    configured: boolean
    /** API Key 是否已填（桌面默认 deepseek 端点 prefill base_url/model，引导按 configured&&hasKey 判定） */
    hasKey: boolean
    provider: string
    baseUrl: string
    model: string
    activeModel: string
    /** 当前生效 max_tokens（默认值 / 设置页覆盖） */
    maxTokens: number
    maxTokensSource: 'default' | 'settings' | string
    maxTokensLabel: string
    defaultMaxTokens: number
  }
  models: { model: string; assets: number }[]
}

export interface TagTestResultView {
  ok: boolean
  model: string
  latencyMs: number
  reply?: string
  error?: string
}

// ---- E5 打标确认流 ----

/** 单条提案（kind: tag | title | titleEn | description） */
export interface TagReviewItem {
  id: number
  kind: 'tag' | 'title' | 'titleEn' | 'description'
  name: string
  value: string
  model: string | null
  taskId: number | null
  taskName: string | null
  score: number
  edited: boolean
}

/** 确认页一张卡 = 一个资产的提案组 */
export interface TagReviewCard {
  assetId: number
  fileName: string
  items: TagReviewItem[]
}

export interface TagReviewList {
  total: number
  items: TagReviewCard[]
}

/** 带错误码的 API 错误（前端按 code 给引导，如 no_vector_index）。 */
export class ApiError extends Error {
  code?: string
  status: number
  /** 后端给的可操作提示（如「去装插件」「改绑规格」）；没有人话提示时为 undefined */
  hint?: string
  /** 界面动作（`open_packs_drawer` = 打开处理插件抽屉）；后端不认识时为 undefined */
  action?: string
  /**
   * 译文占位符参数（i18n-E2，卡 t_63c5b793）：后端 `params` 字段原样透传，
   * 是 `errors.<code>` 词条里 `{name}` 插值的取值源（仅 string/number）。
   */
  params?: Record<string, string | number>
  constructor(
    message: string,
    status: number,
    code?: string,
    hint?: string,
    action?: string,
    params?: Record<string, string | number>,
  ) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.hint = hint
    this.action = action
    this.params = params
  }
}

/**
/**
 * 错误文案单通道（i18n-E2 卡 t_63c5b793 + i18n-G 合并卡 t_25f3b8c8 收敛 D 侧 6 条兜底映射）：
 * `code` 存在且词典有 `errors.<code>` 词条 → 按 locale 译文 + `params` 插值；
 * 查不到（zh 薄壳 / 未收录码）→ 后端中文原句兜底。
 * 判「有没有词条」利用 tGlobal 缺 key 原样返回 key 的行为（zh 薄壳不收 errors.* ⇒ 恒回落）。
 */
export function apiErrorMessage(e: unknown): string {
  const fallback = e instanceof Error ? e.message : String(e)
  if (!(e instanceof ApiError) || !e.code) return fallback
  const key = `errors.${e.code}`
  const translated = tGlobal(key, e.params)
  return translated === key ? fallback : translated
}

/** 后端 params → TParams（只留 string/number；其余类型丢弃，不让脏值进插值） */
function coerceParams(raw: unknown): Record<string, string | number> | undefined {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return undefined
  const out: Record<string, string | number> = {}
  for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
    if (typeof v === 'string' || typeof v === 'number') out[k] = v
  }
  return Object.keys(out).length > 0 ? out : undefined
}

async function jfetch<T>(url: string, init?: RequestInit): Promise<T> {
  // i18n-F：统一注入请求语言（后端按它取包自带文案；切语言后下一次请求生效，
  // 响应里的 `*I18n` 原样对象让前端即时切换，不依赖重拉）
  const headers = new Headers(init?.headers)
  headers.set('accept-language', getGlobalLocale())
  const r = await fetch(url, { ...init, headers })
  if (!r.ok) {
    let msg = `${r.status} ${r.statusText}`
    let code: string | undefined
    let hint: string | undefined
    let action: string | undefined
    let params: Record<string, string | number> | undefined
    try {
      const j = await r.json()
      if (j && typeof j.error === 'string') msg = j.error
      if (j && typeof j.code === 'string') code = j.code
      params = coerceParams(j?.params)
      // 判定前置错误带「去哪儿补救」（N18）：hit 存在时才用 message 人话
      // （老接口的 `{error, message}` 里 error 是码、message 是补充；那里仍显示 error，行为不变）
      if (j && typeof j.hint === 'string') {
        hint = j.hint
        if (typeof j.message === 'string' && j.message) msg = j.message
      }
      if (j && typeof j.action === 'string') action = j.action
    } catch {
      /* keep status text */
    }
    // 错误码单通道：词典命中 → 译文插值；否则保持后端原句（中文兜底）
    msg = apiErrorMessage(new ApiError(msg, r.status, code, hint, action, params))
    throw new ApiError(msg, r.status, code, hint, action, params)
  }
  return r.json() as Promise<T>
}

function post<T>(url: string, body: unknown): Promise<T> {
  return jfetch<T>(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
}

function put<T>(url: string, body: unknown): Promise<T> {
  return jfetch<T>(url, {
    method: 'PUT',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
}

function del<T>(url: string): Promise<T> {
  return jfetch<T>(url, { method: 'DELETE' })
}

/** EXIF2：回填队列状态（GET /api/exif/status） */
export interface ExifStatus {
  jobDone: number
  jobFailed: number
  jobPending: number
  /** 仍未有结论（exif 为 null）的图片数 */
  remaining: number
  lastError: string | null
  parseOnImport: boolean
  gpsMode: 'off' | 'coords' | 'place'
  gpsProviderConfigured: boolean
}

/** EXIF2：EXIF/GPS 设置（PUT /api/settings/exif） */
export interface ExifSettings {
  parseOnImport: boolean
  gps: 'off' | 'coords' | 'place'
  gpsProvider: { name: string; apiKey: string; baseUrl: string }
}

export type AssetQuery = {
  mediaType?: string
  missing?: number
  /** D3 无法预览：1=只看解码失败；0=只看可预览 */
  broken?: 0 | 1
  ext?: string
  tag?: string
  dirPrefix?: string
  untagged?: boolean
  recent?: boolean
  colorLabel?: string
  /** E4：assetId 集合（相似结果临时集合回读） */
  ids?: number[]
  /** EXIF2：拍摄时间范围（unix 秒，含端点） */
  takenFrom?: number
  takenTo?: number
  cameraModel?: string
  lensModel?: string
  isoMin?: number
  isoMax?: number
  /** EXIF2：画幅过滤 */
  aspect?: 'landscape' | 'portrait' | 'square'
  /** EXIF2：排序键 takenAt（NULL 殿后）；缺省 importedAt */
  sort?: 'importedAt' | 'takenAt'
  /**
   * C7 关键词全库检索：文件名 / 标题（中英）/ 描述 / 路径 的大小写不敏感子串。
   * 为什么放服务端：客户端只能筛已加载的那一页（60 张），全库命中的图在首页之后永远搜不到。
   */
  q?: string
  limit?: number
  offset?: number
}

// ---- C6 合规（处理插件 / 判定三态 / 人工改判） ----
//
// 三态口径（0016 迁移 + 用户裁决）：`pass` 合格 / `fail` 不合格 / `undetermined` 未判定
// （只表示判定过程出错）+ `unjudged` 当前插件下还没跑过判定。`fixed` / `pending_human` 两个旧词
// 已从服务端输出里消失（读侧仅对旧链接兼容，`compliance_view::parse` 仍接受）。

/** 单资产合规状态词（与后端 AssetComplianceState 四档一一对应；UI 只展示三档 + 未跑） */
export type ComplianceStateWord = 'pass' | 'fail' | 'undetermined' | 'unjudged'

/** 处理插件状态（GET /api/compliance/status → pluginState；packs → plugins[].runtime 同源） */
export interface CompliancePluginState {
  /** 插件是否可用（只读 manifest 判定，不拉子进程） */
  available: boolean
  id?: string
  version?: string
  pluginHash?: string
  apiVersion?: string
  deterministic?: boolean
  signatureStatus?: 'unsigned' | 'declared' | 'verified' | 'invalid'
  timeoutSecs?: number
  capabilities?: string[]
  dir?: string | null
  /** 不可用原因词（plugin_pack_missing | plugin_manifest_invalid | plugin_meta_invalid） */
  reason?: string
  /** 人读解释（可用时为 null）；`plugin_meta_invalid` 时点名 rules.json 里的坏键 */
  note?: string | null
}

/** IP-K08：签名状态（migration 0026 的 signature_status 闭集） */
export type ComplianceSignatureStatus = 'unsigned' | 'declared' | 'verified' | 'invalid'

/** IP-K08：插件包的一个版本行（GET /api/compliance/packs → plugins[].versions） */
export interface CompliancePluginVersionRow {
  version: string
  dir: string
  /** 版本目录还在（卸载只删目录，账本行留审计 → present=false） */
  present: boolean
  /** 该版本 manifest 的能力名（DM-B1：包自己给；读不到 manifest 时 null） */
  name?: string | null
  manifestHash: string | null
  apiVersion: string | null
  deterministic: boolean | null
  signatureStatus: ComplianceSignatureStatus | null
  installedAt: number | null
  /** 账本里的 current 且目录在 */
  current: boolean
  /** false = 目录在但没登记（手工拷进来的，activate 时会补登记） */
  registered?: boolean
  /**
   * manifest 种类（N5b，judge/process 闭集；账本行与目录行都带）。
   * 当前生效版本行的 outputs = 这个能力的动词口径（P3 选择集入口用）。
   */
  outputs?: string[]
}

/** IP-K08：已安装的**代码插件**包（`.dampack`）：版本 / 运行态 / 签名状态 / 是否内置 */
export interface ComplianceInstalledPluginPack {
  id: string
  /**
   * 能力名（DM-B1 / KI-36）：**包自己给** —— manifest 的 `short_name`，老包缺省回落 `name`；
   * 包目录不在 / manifest 不可解析时 null（界面退显 id，缺口一眼可见）。
   * 界面**不再**维护 id → 名字的映射表（KI-36：那份硬编码已删）。
   */
  name?: string | null
  /** 名字的原样 i18n 对象（i18n-F；包写对象形态时才有，前端 pickI18n 切语言即时生效） */
  nameI18n?: PackText
  /** 一句话「做什么 + 有什么用」（manifest `summary`，未声明 = null，不编造） */
  summary?: string | null
  /** 描述的原样 i18n 对象（i18n-F；同 nameI18n 口径） */
  summaryI18n?: PackText
  /** 随产品分发（不可卸载；升级 = 装新版本 + 切 current） */
  builtin: boolean
  /** 当前生效版本（目录不在 = null） */
  currentVersion: string | null
  /** 指针指向已被删掉的目录（该版本被卸载过）—— 界面应提示重装 */
  staleCurrent: string | null
  versions: CompliancePluginVersionRow[]
  /** 与 /api/compliance/status.pluginState 同源语义（只读 manifest，不拉子进程） */
  runtime: CompliancePluginState
}

/** IP-K08：安装期签名策略（配了信任公钥 = 强制验签） */
export interface ComplianceSignaturePolicyView {
  required: boolean
  trustedKeys: number
}

/** POST /api/compliance/packs/install */
export interface CompliancePackInstallResult {
  ok: boolean
  id: string
  version: string
  dir: string
  manifestHash: string
  apiVersion: string
  deterministic: boolean
  capabilities: string[]
  signatureStatus: ComplianceSignatureStatus
  signaturePolicy: ComplianceSignaturePolicyView
  /** 归档带 SHA256SUMS 且逐文件核对通过 */
  digestsChecked: boolean
  /** true = 同 id@version 同内容已装过（幂等，没有新副本目录） */
  reused: boolean
  activated: boolean
  previousVersion: string | null
  currentVersion: string | null
  filename?: string | null
  source: 'path' | 'upload'
}

/** POST /api/compliance/packs/{id}/activate | /rollback */
export interface CompliancePackSwitchResult {
  ok: boolean
  id: string
  mode: 'activate' | 'rollback'
  from: string | null
  to: string
  version: string
  dir: string
  pluginHash: string
  apiVersion: string
  deterministic: boolean
  capabilities: string[]
  installedVersions: string[]
}

/** DELETE /api/compliance/packs/{id} */
export interface CompliancePackUninstallResult {
  ok: boolean
  id: string
  removed: string[]
  remaining: string[]
  currentVersion: string | null
  staleCurrent: string | null
  note: string
}

/** GET /api/compliance/packs/{id}（插件包详情；`?probe=1` 才有 packInfo） */
export interface CompliancePluginPackDetail {
  id: string
  source: 'plugin'
  builtin: boolean
  active: boolean
  currentVersion?: string | null
  version?: string
  dir?: string
  /** manifest.json 原文结构（装载失败时为 null + error） */
  manifest: Record<string, unknown> | null
  pluginHash?: string
  signatureStatus?: ComplianceSignatureStatus
  signaturePolicy?: ComplianceSignaturePolicyView
  /** `resources/rules.json` 的 rules 段（静态读，不拉子进程） */
  rules?: { id: string; name?: PackText; [k: string]: unknown }[] | null
  /** 包自己报的规则清单（`?probe=1` 时拉 `pack.info`） */
  packInfo?: Record<string, unknown> | null
  packInfoError?: string | null
  versions?: CompliancePluginVersionRow[]
  error?: string | null
}

// ---------------------------------------------------------------- N2/N1：参数 schema + 规格

/**
 * 参数声明（`GET /api/plugins/{id}/schema`，K07-I `t_9b9f9968`）。
 *
 * 三态都给结构，界面不用猜：
 * - 包在、声明了 → `declared:true` + `schema`（插件原文，`ParamForm` 据此画控件）；
 * - 包在、没声明（老包）→ `declared:false` + `schema:null`（协议 v1.1 自由透传 → 空面板）；
 * - 包不在 → 404 `plugin_not_found`（**不是**空 schema —— 那会让界面以为「这插件没参数」）。
 */
export interface PluginParamsSchemaView {
  pluginId: string
  version: string
  pluginHash: string
  declared: boolean
  schemaFile: string | null
  schema: ParamSchemaDoc | null
}

/** 参数类型闭集（数据语义；控件映射由宿主唯一一套，插件不能指定控件）。 */
export type ParamType =
  | 'integer'
  | 'number'
  | 'boolean'
  | 'enum'
  | 'string'
  | 'color'
  | 'asset_ref'
  | 'range'

export interface ParamProperty {
  type: ParamType
  title?: string
  default?: unknown
  min?: number
  max?: number
  step?: number
  /** enum 的取值集合（必须落在里面） */
  options?: (string | number)[]
  unit?: string
  group?: string
  advanced?: boolean
  /** 条件显隐：另一个参数等于某值才显示（只此一种，不做表达式） */
  when?: Record<string, (string | number | boolean)[]>
  /** 危险参数：不可逆 → 二次确认 */
  irreversible?: boolean
  /** 危险参数：改图本体 → 只允许在工作版本上改 */
  affectsBody?: boolean
  /** asset_ref 的可选 MIME 白名单 */
  accept?: string[]
}

/** `params.schema.json`（schemaVersion 1；属性平铺一层，嵌套对象 v1 没做）。 */
export interface ParamSchemaDoc {
  schemaVersion?: number
  groups?: { key: string; title: string; collapsed?: boolean }[]
  properties: Record<string, ParamProperty>
  required?: string[]
}

/** 规格读接口现算的插件现状（**不落库**：装/卸是环境事实，不是规格属性）。 */
export interface ComplianceSpecPluginStatus {
  installed: boolean
  version: string | null
  /** 钉了版本而实际版本不同 */
  versionMismatch: boolean
  /** 插件是否声明了参数 schema（false = 老包：层原样透传） */
  paramsSchemaDeclared: boolean
  /** 包声明的规则 id（空 = 无法校验规则名） */
  rulesAvailable: string[]
  note: string | null
}

/**
 * 规格（`plugin_spec` 一行）：要做到什么标准，不安装、不分发。
 *
 * N链合流：**只有一份规范结构** —— 就是下文 `SpecView`（N12 的界面口径，含动作链强类型）。
 * 这里不再另立一套（曾并行存在 `ComplianceSpecView`，`actions` 一边 `unknown[]` 一边
 * `SpecActionView[]`，两套口径必然在界面上打架）。
 */
export type ComplianceSpecView = SpecView

export interface ComplianceSpecList {
  total: number
  items: ComplianceSpecView[]
}

// 入参是 **camelCase**：后端 `CreateReq/PatchReq` 带 `#[serde(rename_all = "camelCase")]`，
// 传 `plugin_id` 会被 422 打回（N4 手验实测：界面点「创建规格」没反应，服务端报 missing field `pluginId`）
export interface ComplianceSpecCreateReq {
  pluginId: string
  name: string
  pluginVersionPin?: string | null
  industry?: string | null
  rules?: string[]
  thresholds?: Record<string, unknown>
  actions?: unknown[]
  output?: Record<string, unknown>
  paramsDiff?: Record<string, unknown>
  enabled?: boolean
}

export interface ComplianceSpecPatchReq {
  name?: string
  pluginVersionPin?: string | null
  industry?: string | null
  rules?: string[]
  thresholds?: Record<string, unknown>
  actions?: unknown[]
  output?: Record<string, unknown>
  paramsDiff?: Record<string, unknown>
  enabled?: boolean
}

export interface ComplianceStates {
  pass: number
  fail: number
  /** 未判定 —— **只表示判定过程出错**（读图失败 / VL 超时或不可解析 / 插件异常） */
  undetermined: number
  /** 当前处理插件下还没跑过判定 */
  unjudged: number
  judged: number
  total: number
}

export interface ComplianceStatusView {
  /** 当前判定口径的插件 id + 规则数（无插件时 rules = 0） */
  pack: { id: string; rules: number }
  /** 实际出判定结论的 packId（改判/历史按它归档；与 pack.id 同源） */
  verdictPackId: string
  /** 处理插件可用性（界面据此显示「插件在跑 / 未安装 / 不可用」） */
  pluginState: CompliancePluginState
  states: ComplianceStates
  /** 低置信度复核队列：total = 需复核项；unresolved = 还没人工改判的；reviewed = 已改判 */
  queue: { total: number; unresolved: number; reviewed: number }
  /** 旧口径误判反馈（只作统计，入口已并入改判；界面不再写它） */
  feedback: { total: number; misjudged: number }
}

export interface ComplianceFeedbackView {
  id: number
  machineResult: 'pass' | 'fail' | 'undetermined'
  humanResult: 'pass' | 'fail'
  reason: string | null
  /** 机器结论与人工结论不一致 = 一次误判记录（转人工处置不算） */
  misjudged: boolean
  ts: number
}

/** 生效中的人工改判（机器结论永不改写，改判只覆盖生效结论） */
export interface HumanReviewView {
  id: number
  /** null = 资产级改判（整张图一条结论） */
  ruleId: string | null
  scope: string
  /** pass | fail —— 人的裁决只有两档 */
  humanStatus: 'pass' | 'fail'
  reason: string
  at: number
  reviewedBy: string | null
  /** 改判当时的机器结论快照（审计对照） */
  machineResult: string | null
  machineConfidence: number | null
}

export interface ComplianceQueueRule {
  ruleId: string
  ruleName: string
  /** 规则名原样 i18n 对象（i18n-F；同 ComplianceVerdictView 口径） */
  ruleNameI18n?: PackText
  /** 机器结论（pass/fail/undetermined/skipped；**永不被改判改写**） */
  result: string
  /** 该规则置信度（null = 0016 前的历史行，不可知） */
  confidence: number | null
  /** 该行是否属于「需要复核」（undetermined 或 confidence < 阈值） */
  lowConfidence: boolean
  /** 生效结论 = humanStatus ?? result（前端直接用，不要自己算） */
  finalStatus: string
  score: number | null
  reason: string | null
  evidence: string | null
  fixAction: string | null
  fixTier: string | null
  recheckResult: string | null
  ts: number
  humanReview: HumanReviewView | null
  /** 旧口径误判反馈（只作统计，不改变结论） */
  feedback: ComplianceFeedbackView | null
}

export interface ComplianceQueueCard {
  assetId: number
  fileName: string
  absPath: string
  thumbPath: string | null
  missing: number
  rules: ComplianceQueueRule[]
  /** 该资产所有待人工项都已处置（有反馈或有生效改判） */
  resolved: boolean
  /** 该资产有**生效改判**（改判才改变结论） */
  reviewed: boolean
  assetHumanReview: HumanReviewView | null
}

export interface ComplianceQueueView {
  total: number
  items: ComplianceQueueCard[]
  packId: string
  threshold: number
  lowConfidenceOnly: boolean
}

/** GET /api/compliance/review —— 复核列表的一行（一个 (asset, rule)；资产级改判 ruleId = null） */
export interface ComplianceReviewItem {
  assetId: number
  fileName: string
  absPath: string
  thumbPath: string | null
  ruleId: string | null
  ruleName: string | null
  /** 规则名原样 i18n 对象（i18n-F；同 ComplianceVerdictView 口径） */
  ruleNameI18n?: PackText
  judgementScope: string | null
  /** 机器结论（pass/fail/undetermined/skipped；资产级 = 机器汇总档位） */
  machineResult: string
  machineConfidence: number | null
  machineScore: number | null
  reason: string | null
  lowConfidence: boolean
  /** 生效结论 = humanStatus ?? machineResult */
  finalStatus: string
  humanReview: HumanReviewView | null
  reviewed: boolean
  ts: number
}

export interface ComplianceReviewListView {
  packId: string
  total: number
  items: ComplianceReviewItem[]
  threshold: number
  lowConfidenceOnly: boolean
}

/** POST /api/compliance/review —— 改判回执（含改判后的资产状态） */
export interface ComplianceReviewResult {
  ok: boolean
  packId: string
  assetId: number
  ruleId: string | null
  machineResult: string | null
  machineConfidence: number | null
  finalStatus: string
  humanReview: HumanReviewView
  assetState: string | null
  assetStateLabel: string | null
  machineState: string | null
  machineStateLabel: string | null
  threshold: number
}

/** GET /api/compliance/review/history —— 改判历史（含撤销/被覆盖） */
export interface ComplianceReviewHistoryItem {
  id: number
  assetId: number
  ruleId: string | null
  scope: string
  groupKey: string | null
  humanStatus: 'pass' | 'fail'
  reason: string
  at: number
  reviewedBy: string | null
  machineResult: string | null
  machineConfidence: number | null
  revokedAt: number | null
  revokeReason: string | null
  active: boolean
}


/** POST /api/compliance/judge —— 产品级判定触发（入队 + 后台跑） */
export interface ComplianceJudgeStartView {
  ok: boolean
  packId: string
  scope: 'asset' | 'group' | 'both'
  /** 本轮候选资产数（按 mode/pack 选出、已排除已删） */
  candidates: number
  /** 实际新入队的逐图 job 数（防重：队列里已有 pending/running 的不重复入队） */
  queuedAssets: number
  /** 实际新入队的组级 job 数 */
  queuedGroups: number
  /** CMP-JOB-01：本次执行（run）的 id —— `/api/compliance/jobs/{runId}` 用它查这一次的明细 */
  runId: number
  /** 从上次取消/中断遗留的 pending 任务里认领过来的条数（>0 = 这次在续跑） */
  adoptedPending: number
  workers: number
  /** 请求了组级但处理插件不支持组级判定 → true（queuedGroups 为 0） */
  groupSkipped?: boolean
}

/** 判定进度/汇总（GET /api/compliance/judge/status；前端 1s 轮询） */
export interface ComplianceJudgeStatusView {
  running: boolean
  cancelling: boolean
  progress: {
    jobId: number
    assetId: number
    status: string
    index: number
    total: number
    error?: string
  } | null
  report: {
    total: number
    done: number
    failed: number
    retry: number
    skipped: number
    cancelled: boolean
    errors: string[]
    ruleErrors: string[]
    groupTotal: number
    groupDone: number
    groupFailed: number
    groupRetry: number
    groupSkipped: number
    groupErrors: string[]
    groupJobDone: number
    groupJobFailed: number
    groupJobPending: number
    jobDone: number
    jobFailed: number
    jobPending: number
  } | null
  error: string | null
  /**
   * **历史累计**（全库口径，不含本 run 的边界）——CMP-JOB-01 起显式标注，
   * 界面要用「这一次」的数字请读 `queueCurrent` / `run`。
   */
  legacyCumulative?: boolean
  queue: {
    assetDone: number
    assetFailed: number
    assetPending: number
    groupDone: number
    groupFailed: number
    groupPending: number
  }
  /**
   * 本 run 口径（正在跑的那一次；没有正在跑的取最近一次）。
   * 注意：`assetDone` / `groupDone` 是**队列行状态计数**（含跳过，跳过与成功同终态）；
   * 用户可见的「成功数 / 跳过数」用 `run.done` / `run.skipped`（两者相加才等于这里的 done）。
   */
  queueCurrent?: {
    runId: number
    assetDone: number
    assetFailed: number
    assetPending: number
    groupDone: number
    groupFailed: number
    groupPending: number
    skipped: number
  } | null
  /** 本 run 的结果统计（runId/status/时间/成功/失败/跳过/总数/耗时） */
  run?: ComplianceJobView | null
}

/**
 * CMP-JOB-01：一次批量审核 = 一个独立 job（run）。历史互不改写、不叠加。
 * 计数口径：job 收工后是快照；运行中是实时队列计数。
 * 恒等式：运行中 `total = done + failed + skipped + queued`；
 * 收工后 `total = done + failed + skipped + cancelled`。
 */
export interface ComplianceJobView {
  runId: number
  /** running | done | cancelled | interrupted | failed */
  status: string
  /** 状态人话（进行中 / 已完成 / 已取消 / 已中断（进程重启）/ 执行失败） */
  statusLabel: string
  startedAt: number
  finishedAt: number | null
  total: number
  /** 成功（不含 skipped） */
  done: number
  failed: number
  /** 跳过（文件已失联 / 不是图片 / 已删除 —— 不是失败） */
  skipped: number
  /**
   * 取消/中断后**没执行**的条数（只有收工的行才可能非零；运行中后端恒给 0）。
   * 别拿它承载「排队中」——那是 `queued`。
   */
  cancelled: number
  /**
   * 运行中：排队中 + 执行中（未结算）的条数；收工后恒 0。
   * 老后端没有该字段 → 按 0 处理（见 `jobCounts`）。
   */
  queued?: number
  durationMs: number | null
  error: string | null
  /**
   * DM-B2（KI-37）：这次涉及多少张图（distinct 资产，判定账本口径）。
   * 处理类 run 也照给（= 执行过几张）；老后端缺字段 → undefined（按 `total` 兜底）。
   */
  totalAssets?: number | null
  /**
   * DM-B2（KI-37）：**一张图一个结论**的三态分布（`fail > undetermined > pass`）。
   * 只有检查类有（处理类没有「合格/不合格」）→ 处理类恒 null。
   */
  verdictCounts?: { pass: number; fail: number; undetermined: number } | null
  params: Record<string, unknown> | null
}

/** GET /api/compliance/jobs —— 执行列表（按开始时间倒序） */
export interface ComplianceJobsView {
  total: number
  limit: number
  offset: number
  jobs: ComplianceJobView[]
}

/** 一次执行内部的 task（= 一个处理步骤） */
export interface ComplianceJobTaskView {
  jobId: number
  targetId: number
  /** 人话步骤名（规则名/阶段名，如「逐图判定 · 规则 F0_domain」） */
  step: string
  assetId: number
  status: string
  statusLabel: string
  attempts: number
  /** 失败原因或跳过原因（`not_image:` / `file_missing:` 前缀 = 跳过） */
  lastError: string | null
  updatedAt: number
}

/** GET /api/compliance/jobs/{runId} —— 一次执行 + task 明细 */
export interface ComplianceJobDetailView {
  run: ComplianceJobView
  taskTotal: number
  limit: number
  offset: number
  tasks: ComplianceJobTaskView[]
}

/** GET /api/compliance/jobs/current —— 正在跑的那一次（没有则 run=null） */
export interface ComplianceJobCurrentView {
  run: ComplianceJobView | null
  queue?: {
    runId: number
    assetDone: number
    assetFailed: number
    assetPending: number
    groupDone: number
    groupFailed: number
    groupPending: number
    skipped: number
    /** 排队中 + 执行中（= assetPending + groupPending；老后端缺字段按 0） */
    queued?: number
    /** 取消后没执行（运行中恒 0；老后端缺字段按 0） */
    cancelled?: number
  }
}

export interface ComplianceVerdictView {
  ruleId: string
  ruleName: string
  /** 规则名原样 i18n 对象（i18n-F：包 rules[].name 写对象形态时才有；切语言不重拉） */
  ruleNameI18n?: PackText
  /** per_asset | per_group（组级判定按成员展开成行） */
  judgementScope: string | null
  groupKey: string | null
  /** 机器结论（永不被改判改写） */
  result: string
  /** 生效结论 = humanStatus ?? result */
  finalStatus: string
  /** 规则置信度（null = 历史行不可知） */
  confidence: number | null
  lowConfidence: boolean
  humanReview: HumanReviewView | null
  score: number | null
  reason: string | null
  evidence: string | null
  fixAction: string | null
  fixTier: string | null
  recheckResult: string | null
  ts: number
  feedback: ComplianceFeedbackView | null
}

/** GET /api/compliance/verdicts?assetId= —— 单资产判定明细（三态 + 改判 + 证据） */
export interface ComplianceVerdictsView {
  packId: string
  assetId: number
  /** 生效档位（含人工改判） */
  state: ComplianceStateWord
  stateLabel: string
  /** 纯机器档位（不含改判）—— 界面要说清「机器判定 X / 改判后 Y」 */
  machineState: ComplianceStateWord
  machineStateLabel: string
  machineConfidence: number | null
  assetHumanReview: HumanReviewView | null
  items: ComplianceVerdictView[]
}

/**
 * PROC-RES-2：处理产物（结果行 `artifacts[]` 的元素）。
 *
 * `kind` 恒 `export_file`（处理产物唯一去处）；`path` 是后端机器上的产物文件路径
 * （`{data_dir}/compliance/fixes/job-<id>/…`），读取/下载走 [`artifactUrl`]。
 */
export interface ArtifactFileView {
  kind: string
  path: string
  sha256?: string | null
  bytes?: number | null
  format?: string | null
  width?: number | null
  height?: number | null
}

/** 产物文件 URL（缩略图/打开共用；`download=true` 走附件下载头） */
export function artifactUrl(path: string, download = false): string {
  const p = `/api/compliance/artifact?path=${encodeURIComponent(path)}`
  return download ? `${p}&download=1` : p
}

/**
 * N5b：结果表（`GET /api/compliance/results`）。
 *
 * 行里 `result` = 执行结果（ok/failed/skipped），`verdict` = 审核结论（只有判定类行有）；
 * `fields` = 行业自定义列的原值（列定义在 `columns`，未声明 = null → 默认列）。
 */
export interface ComplianceResultRowView {
  assetId: number
  fileName: string
  absPath: string
  ruleId: string
  judgementScope: string | null
  result: 'ok' | 'failed' | 'skipped'
  verdict: {
    status: 'pass' | 'fail' | 'undetermined'
    confidence: number | null
    reasonCode: string | null
  } | null
  reason: string | null
  fields: Record<string, unknown>
  /** 处理类行才有（export_file 契约）；判定类行 = null */
  artifacts?: ArtifactFileView[] | null
  ts: number
}

export interface ComplianceResultsView {
  packId: string
  /** 插件种类（安装记录口径）：judge 才画审核列 */
  outputs: ('verdict' | 'artifact')[]
  /** 单资产队列行（诊断口径；任务中心用下面的 runId） */
  jobId: number | null
  /** 一次执行（`/api/compliance/jobs` 的 runId）；按执行过滤时回显 */
  runId?: number | null
  total: number
  limit: number
  offset: number
  /** 插件声明的列；null = 未声明 → 默认列 */
  columns: {
    key: string
    label: string
    type: 'number' | 'text' | 'enum' | 'bool'
    unit?: string | null
    order?: number
  }[] | null
  /** 被丢弃的列声明（不许让整页报错，只提示） */
  columnWarnings: string[]
  items: ComplianceResultRowView[]
}

/**
 * N12：**规格**（`GET /api/specs`）= 用户嘴里的「方案」：选哪些规则 + 阈值 + 动作链 + 输出。
 *
 * 不安装、不分发；同一能力下可以有多条（淘宝主图 / 亚马逊主图 / 我的 800px webp）。
 * 界面判「检查 / 处理」只看一件事：**有没有动作链** —— 有动作链 = 处理类，没有 = 检查类。
 * 这是规格数据本身的性质（judge 规格带 rules/thresholds，process 规格带 actions），
 * 不靠插件目录猜、也不引入新字段。
 */
export interface SpecActionView {
  action: string
  params?: Record<string, unknown>
  when?: unknown
}

export interface SpecView {
  specId: number
  pluginId: string
  pluginVersionPin: string | null
  name: string
  industry: string | null
  rules: string[]
  thresholds: Record<string, unknown>
  actions: SpecActionView[]
  output: Record<string, unknown>
  paramsDiff: Record<string, unknown>
  /** 0 = 停用（可见可编辑，但不许被任务引用） */
  enabled: 0 | 1
  originKey: string | null
  createdAt: number
  updatedAt: number
  /** 现算的环境事实（装了/卸了/版本漂移），不落库 */
  pluginStatus: {
    installed: boolean
    /** 能力名（DM-B1：包自己给，`short_name` → `name`；包不在 = null） */
    pluginName?: string | null
    /** 一句话「做什么 + 有什么用」（manifest `summary`；未声明 = null） */
    pluginSummary?: string | null
    version: string | null
    versionMismatch: boolean
    paramsSchemaDeclared: boolean
    rulesAvailable: string[]
    note: string | null
  }
}

/** E3 智能文件夹（保存搜索）DTO */
export interface SmartFolderDto {
  id: number
  name: string
  query: string
  createdAt: number
  updatedAt: number
}

/** E4 批量操作单条回执（后端 AssetOpResult） */
export interface OpResult {
  assetId: number
  ok: boolean
  detail: string
  error: string | null
}

export interface BatchOpResult {
  results: OpResult[]
  okCount: number
  failCount: number
}

/** E2 导出：进度 / 报告（与 core-types::ExportProgress / ExportReport 同形） */
export interface ExportProgress {
  phase: string
  done: number
  total: number
  current?: string | null
}

export interface ExportSkipped {
  assetId: number
  fileName: string
  reason: string
}

export interface ExportReport {
  total: number
  exported: number
  /** KI-15：选择集内拒选（approvalStatus=rejected）被排除的张数——拒选 = 不交付，无开关 */
  excludedRejected: number
  skipped: ExportSkipped[]
  errors: string[]
  cancelled: boolean
  dest: string
  manifestWritten: boolean
}

export interface ExportStatus {
  running: boolean
  progress: ExportProgress | null
  report: ExportReport | null
  error: string | null
}

/**
 * 「按过滤导出」的载荷形态（W/B 布局收编后唯一入口 = 全局导出 Dialog 的「当前筛选结果」范围）。
 *
 * 与 `/api/library/assets` 的查询条件同形（后端 ImportFilter），但**刻意剔除**四个键：
 *  - `limit` / `offset`：分页不是筛选，混进导出载荷只会制造歧义；
 *  - `recent`：后端 ImportFilter 没有这个字段 —— serde 默认静默忽略未知键，
 *    `recent:true` 一旦漏进来就是「导出整库」级别的静默错误（DT4 同类教训）；
 *  - `ids`：集合导出走顶层 assetIds，不许藏在 filter 里。
 */
export type ExportFilterPayload = Omit<AssetQuery, 'limit' | 'offset' | 'recent' | 'ids'>

export interface ExportRequest {
  /** 选择集（图库/搜索勾选导出走这个）；与 filter 二选一 */
  assetIds?: number[]
  /** 查询过滤导出（全局 Dialog 的「当前筛选结果」范围；空对象 = 显式整库） */
  filter?: ExportFilterPayload
  /** original / thumb256 / thumb1024 */
  content: string
  destDir?: string
  zipPath?: string
  manifest: boolean
}

// ---- STORAGE-MIGRATE-1（DT7 前端 / DT6 后端）：一键迁入库 reference → copyin ----

/** `GET /api/library/migrate-to-library/status` 的进度计数（只读，不含逐条回执）。 */
export interface MigrateProgress {
  total: number
  processed: number
  migrated: number
  skipped: number
  failed: number
  bytes: number
}

/** 跳过明细：`reason` 是后端稳定 code（`skip_reason_text` 出人话）。 */
export interface MigrateSkip {
  assetId: number
  reason: string
  detail: string
}

/** 失败明细（真错误：复制/rename/DB 写失败；资产保持原样）。 */
export interface MigrateFailure {
  assetId: number
  code: string
  detail: string
}

/** 告警（不影响结论，例如勾了删源但源删不掉）。 */
export interface MigrateWarning {
  assetId: number
  detail: string
}

export interface MigrateReport {
  batchId: string
  /** 本次请求的 id 数（含重复入参） */
  total: number
  migrated: number
  skipped: MigrateSkip[]
  failed: MigrateFailure[]
  /** 本次真正写进库根的字节数（续跑命中 = 0） */
  bytes: number
  warnings: MigrateWarning[]
}

/** 状态快照：没有任务 / batchId 不匹配时后端只回 `{state:"idle"}`。 */
export interface MigrateStatus extends Partial<MigrateProgress> {
  state: 'idle' | 'running' | 'done' | 'failed'
  batchId?: string
  startedAt?: number
  finishedAt?: number
  /** state=failed 的整批错误（单条失败进 report.failed，不写这里） */
  error?: string
  report?: MigrateReport
}

/** `POST /api/library/migrate-to-library` 的 202 回执（后台跑，进度走 status）。 */
export interface MigrateDispatch {
  batchId: string
  state: 'running'
  total: number
}

export interface MigrateRequest {
  /** 非空、≤900（后端 MIGRATE_MAX_IDS） */
  ids: number[]
  /** 复制成功后删源文件；默认 false（**源默认保留**） */
  deleteSource?: boolean
  batchId?: string
  /** 审计里的「人」；空 = local */
  operator?: string
}

// ---- W5 去重（fileHash 精确 ∪ pHash 近似） ----

export interface DupMemberView {
  assetId: number
  fileName: string
  absPath: string
  fileSz: number
  importedAt: number
  fileHash: string | null
  pHash: string | null
  width: number | null
  height: number | null
  starRating: number
  rejected: boolean
  /** 该成员在组内的最高视觉相似度（判重塔向量）；null = 只靠 fileHash/pHash 进组 */
  similarity?: number | null
}

/**
 * KI-2b：kind 分四档（两套判据分层合并，组内取**最保守**层）：
 * - exact       = 同文件（fileHash 全同）
 * - near        = pHash 汉明 ≤8 连通
 * - near_visual = 判重塔向量余弦 ≥0.98（同一视觉内容）
 * - variant     = 判重塔向量余弦 [0.90,0.98)（同图不同构图，需二次确认）
 * `mixed` = 组内混了不止一层判据；`layers` 标出命中的判据层。
 */
export type DupGroupKind = 'exact' | 'near' | 'near_visual' | 'variant'

export interface DupGroupView {
  groupId: number
  kind: DupGroupKind
  size: number
  suggestedKeep: number
  reclaimBytes: number
  mixed?: boolean
  layers?: string[]
  members: DupMemberView[]
}

/** 判重进度（KI-2b）：vectors == 0 → 未检测（不等于「无重复」） */
export interface DupJudgeStatus {
  model: string
  vectors: number
  edges: number
  backfillDone: boolean
  backfillTruncated: boolean
  lastJudgeAt: number | null
}

export interface DupGroupsResponse {
  totalGroups: number
  totalAssets: number
  reclaimBytes: number
  kindCounts?: Record<string, number>
  judge?: DupJudgeStatus
  groups: DupGroupView[]
  /** 生效的范围（服务端规范化串：all / dir:<路径> / ids:<a,b> / batch:<id>） */
  range?: string
  /** 范围裁剪的附带事实（范围外还有重复时必须显式说明，不许说「没有重复」） */
  rangeMeta?: DupRangeMeta
  /** 已被人工忽略的组数（刷新后不复现） */
  ignoredCount?: number
}

/** KI-2/KI-7：范围裁剪口径（与 `crates/app-web/src/dedup_api.rs::Range` 对齐） */
export type DupRangeKind = 'all' | 'dir' | 'ids' | 'batch'

export interface DupRangeMeta {
  kind: DupRangeKind
  /** dir 生效的目录前缀；batch 走老库近似回退时才是批次 rootPath */
  prefix?: string | null
  /** 被裁掉的成员数（在库里重复但不在本范围内） */
  outOfScopeAssets: number
  /** ids 范围：选择集里的图在库内仍有重复，但重复对象在选择集外（本范围看不到） */
  outsideDupAssets: number
  /**
   * DM-A4：batch 范围是否**近似**。true = 该批名下没有任何 `batchId` 资产（0033 之前入库的老库），
   * 服务端回退成 rootPath 前缀过滤 —— 与 `prefix` 一起读，不许当「本批精确」。
   */
  approx?: boolean
  /** DM-A4：batch 范围本批（未被回收站拿走）的资产数；非 batch 范围为 null */
  batchAssets?: number | null
}

/** 库外文件夹只读扫描（不入库）的成员/组/报告 —— `POST /api/dedup/scan` */
export interface DupScanMember {
  path: string
  fileName: string
  fileSz: number
  fileHash: string
  pHash: string
  keep: boolean
  /** 与组内保留项的关系（如「无变换（汉明距离 0）」） */
  relation: string | null
}

export interface DupScanGroup {
  groupId: number
  kind: 'exact' | 'near'
  size: number
  reclaimBytes: number
  evidence: string[]
  members: DupScanMember[]
}

export interface DupScanReport {
  scanId: string
  path: string
  scanned: number
  truncated: boolean
  pendingFiles: number
  unreadable: { path: string; error: string }[]
  totalGroups: number
  reclaimBytes: number
  kindCounts: Record<string, number>
  groups: DupScanGroup[]
  elapsedMs: number
  generatedAt: number
  algorithm: string
  thresholds: string
  note: string
}

export interface AppInfo {
  name: string
  version: string
  dataDir: string
  modelsRoot: string
  registryModels: string[]
}

/**
 * WEBCAP：单个库根的**实测**状态（可写性 = 服务端真建临时文件验过；device = st_dev）。
 * 只读根也会出现在列表里 —— 前端据此逐目录灰置拖放目标，而不是整块隐藏。
 */
export interface RootInfo {
  path: string
  writable: boolean
  code?: string
  raw?: string
  device?: number
}

/** 用户可见能力开关（false = 本形态不承诺该操作，UI 灰置/隐藏）。 */
export interface Features {
  moveAsset: boolean
  renameAsset: boolean
  relink: boolean
  openInOS: boolean
  physicalDelete: boolean
  copyInImport: boolean
  /**
   * 能否读用户本机任意路径（库外文件夹只读查重）。
   * 容器/网页形态 false（「本地路径」是服务器磁盘）→ 入口**隐藏**，不灰置。
   */
  scanExternalPath: boolean
}

/** KI-9/KI-10：`POST /api/os/reveal` 的成功响应（`label` 是服务端给的统一文案）。 */
export interface OsRevealResult {
  ok: boolean
  path: string
  mode: 'reveal' | 'open'
  /** 「在文件夹中显示」/「打开文件夹」——前端按钮文案直接用它，别自己再写一份 */
  label: string
  /** 实际拉起的系统命令（排查/取证用；页面上不展示） */
  program: string
  args: string[]
}

/** `GET /api/capabilities` 契约（服务端单一事实源；前端只读，不自己判形态）。 */
export interface Capabilities {
  /** 桌面形态（Tauri 壳 / local-infer 构建 / DAM_WEB_FORM=desktop 验收钩子） */
  isDesktop: boolean
  /** 库文件归属口径：local-disk（用户本机磁盘）/ server-volume（服务器卷） */
  storeMode: string
  /** 可写库根（roots 里 writable=true 的 path，顺序稳定） */
  writableRoots: string[]
  roots: RootInfo[]
  features: Features
  /** 被禁用能力的人话原因（只含 false 的项，复用「网页版不支持…请在桌面版操作」口径） */
  featureReasons: Partial<Record<keyof Features | string, string>>
}

export const api = {
  // ---- 导入链路 ----
  scan: (path: string) => post<ScanResult>('/api/import/scan', { path }),
  importStart: (path: string, storeMode: 'reference' | 'copyin', autoTag = false) =>
    post<{ batchId: number }>('/api/import/start', { path, storeMode, autoTag }),
  importResume: (batchId: number) => post<{ batchId: number }>('/api/import/resume', { batchId }),
  /** 只重跑上次失败的这些文件（不重扫目录） */
  importRetryFailed: (batchId: number, paths: string[]) =>
    post<{ batchId: number; paths: number }>('/api/import/retry-failed', { batchId, paths }),
  importCancel: () => post<{ ok: boolean }>('/api/import/cancel', {}),
  importStatus: () => jfetch<ImportStatus>('/api/import/status'),
  importBatches: () => jfetch<ImportBatchRecord[]>('/api/import/batches'),

  // ---- 图库读模型 ----
  assets: (q: AssetQuery) => {
    const p = new URLSearchParams()
    for (const [k, v] of Object.entries(q)) {
      if (v !== undefined && v !== '') p.set(k, String(v))
    }
    return jfetch<{ total: number; items: AssetView[] }>(`/api/library/assets?${p}`)
  },
  stats: () => jfetch<LibraryStats>('/api/library/stats'),
  dirs: () => jfetch<{ dirs: DirEntry[]; roots: string[] }>('/api/library/dirs'),
  tags: (limit = 30) => jfetch<TagCount[]>(`/api/library/tags?limit=${limit}`),
  /** 标签候选（输入即查，t_4f145794/t_d4895738）：前缀范围匹配 + 频次 + total/hasMore。
   *  `prefix` 为空且只给 `key` = 列出该 key 下全部值；`offset` 用于翻页（排序 count DESC, value ASC）。 */
  tagCandidates: (prefix: string, opts: { limit?: number; key?: string; offset?: number } = {}) => {
    const p = new URLSearchParams()
    if (prefix) p.set('prefix', prefix)
    if (opts.key) p.set('key', opts.key)
    p.set('limit', String(opts.limit ?? 30))
    if (opts.offset) p.set('offset', String(opts.offset))
    return jfetch<TagCandidatesResp>(`/api/library/tags?${p.toString()}`)
  },
  assetTags: (assetId: number) => jfetch<TagRecord[]>(`/api/library/assets/${assetId}/tags`),
  /** 打标任务结果（维度名: 输出原文），与标签值分开取 */
  assetTagResults: (assetId: number) =>
    jfetch<TagResultRecord[]>(`/api/library/assets/${assetId}/tag-results`),

  // ---- 标签总览（t_cafb2e45，`/tags` 页面）----
  /** 左栏 key 分组（值数）；key='' = 无前缀裸值组 */
  tagGroups: () => jfetch<TagGroupsResp>('/api/library/tag-groups'),
  /**
   * 值列表一页（`page=1` 分支）：key / keyless / prefix 三选一。
   * `after` = 上一页最后一行 → keyset 续页（排序 `count DESC, value ASC`；keyless 按 value ASC）。
   * 与 `tagCandidates` 不同：这里必须带范围条件（全量按计数排序代价过高，服务端直接 400）。
   */
  tagPage: (q: {
    key?: string
    prefix?: string
    keyless?: boolean
    limit?: number
    offset?: number
    after?: { count: number; value: string } | null
  }) => {
    const p = new URLSearchParams()
    p.set('page', '1')
    if (q.keyless) p.set('keyless', '1')
    else if (q.key) p.set('key', q.key)
    if (q.prefix) p.set('prefix', q.prefix)
    p.set('limit', String(q.limit ?? 50))
    if (q.offset) p.set('offset', String(q.offset))
    if (q.after) {
      p.set('afterCount', String(q.after.count))
      p.set('afterValue', q.after.value)
    }
    return jfetch<TagCandidatesResp>(`/api/library/tags?${p.toString()}`)
  },
  /** 每值样本图（最多 3 张） */
  tagSamples: (values: string[], limit = 3) =>
    post<{ items: { value: string; assetIds: number[] }[] }>('/api/library/tag-samples', {
      values,
      limit,
    }),
  /** 合并预览：只算不写（确认框里的数字由此而来，与执行同一段后端代码） */
  tagMergePreview: (values: string[], into: string) =>
    post<TagMergeOutcome>('/api/library/tags/merge-preview', { values, into }),
  tagMerge: (values: string[], into: string) =>
    post<TagMergeOutcome>('/api/library/tags/merge', { values, into }),
  tagRename: (from: string, to: string) =>
    post<TagMergeOutcome>('/api/library/tags/rename', { from, to }),
  tagDelete: (values: string[]) =>
    post<TagDeleteOutcome>('/api/library/tags/delete', { values }),

  // ---- EXIF2 相机聚合（筛选下拉） ----
  cameras: (limit = 50) =>
    jfetch<{ cameras: { model: string; count: number }[] }>(`/api/library/cameras?limit=${limit}`),

  // ---- EXIF2 元数据回填 ----
  exifStatus: () => jfetch<ExifStatus>('/api/exif/status'),
  exifBackfill: (opts: { fromId?: number; toId?: number; limit?: number } = {}) =>
    post<{
      enqueued: number
      processed: number
      done: number
      failed: number
      errors: string[]
    }>('/api/exif/backfill', opts),
  exifRetryFailed: () =>
    post<{ revived: number; processed: number; done: number; failed: number }>(
      '/api/exif/retry-failed',
      {},
    ),
  exifSettings: () => jfetch<ExifSettings>('/api/settings/exif'),
  exifSettingsSave: (s: ExifSettings) =>
    jfetch<ExifSettings>('/api/settings/exif', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(s),
    }),

  // ---- E3 智能文件夹（保存搜索） ----
  smartFolders: () => jfetch<SmartFolderDto[]>('/api/smart-folders'),
  smartFolder: (id: number | string) => jfetch<SmartFolderDto>(`/api/smart-folders/${id}`),
  smartFolderCreate: (name: string, query: object) =>
    post<SmartFolderDto>('/api/smart-folders', { name, query }),
  smartFolderUpdate: (id: number, patch: { name?: string; query?: object }) =>
    jfetch<SmartFolderDto>(`/api/smart-folders/${id}`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(patch),
    }),
  smartFolderDelete: (id: number) =>
    jfetch<{ ok: boolean }>(`/api/smart-folders/${id}`, { method: 'DELETE' }),
  // ---- E2 批量打分 / 拒选 ----
  rate: (ids: number[], rating: number) => post<{ updated: number }>('/api/library/rate', { ids, rating }),
  reject: (ids: number[], rejected = true) =>
    post<{ updated: number }>('/api/library/reject', { ids, rejected }),
  // ---- P1-4 删除 / 撤销：软删进回收站（deleted=1，源文件不动），可恢复 ----
  // 与去重裁决 /api/dedup/resolve + /api/dedup/restore 同一套 svc-library 语义。
  libraryDelete: (ids: number[]) => post<{ deleted: number }>('/api/library/delete', { ids }),
  libraryRestore: (ids: number[]) => post<{ restored: number }>('/api/library/restore', { ids }),

  // ---- E4 批量标签 / 色标 / 移动（逐条回执，部分失败不静默） ----
  /** values = 标签值本身（tag 表只有值，没有维度名） */
  addTags: (ids: number[], values: string[]) =>
    post<BatchOpResult>('/api/library/tag', { ids, values }),
  removeTag: (assetId: number, value: string) =>
    post<{ removed: number }>('/api/library/untag', { assetId, value }),
  setColor: (ids: number[], color: string | null) =>
    post<{ updated: number }>('/api/library/color', { ids, color }),
  moveAssets: (ids: number[], targetDir: string) =>
    post<BatchOpResult>('/api/library/move', { ids, targetDir }),
  // ---- E2 导出（复用 D11 svc-library::export） ----
  exportStart: (req: ExportRequest) => post<{ ok: boolean; total: number }>('/api/export/start', req),
  exportStatus: () => jfetch<ExportStatus>('/api/export/status'),
  exportCancel: () => post<{ ok: boolean }>('/api/export/cancel', {}),

  // ---- W5 去重（分组 / 组裁决 / 回收站） ----
  dupGroups: (range?: string) =>
    jfetch<DupGroupsResponse>(
      range && range !== 'all'
        ? `/api/dedup/groups?range=${encodeURIComponent(range)}`
        : '/api/dedup/groups',
    ),
  /** KI-2：组级忽略（刷新后不复现；成员变化会重新出现） */
  dedupIgnore: (groupIds: number[]) =>
    post<{ ok: boolean; ignored: string[] }>('/api/dedup/ignore', { groupIds }),
  dedupUnignore: (signatures: string[]) =>
    post<{ ok: boolean; removed: number }>('/api/dedup/unignore', { signatures }),
  /** KI-7：库外文件夹只读查重（不入库、不写向量；结果只能看与导出） */
  dedupScan: (path: string, maxFiles?: number) =>
    post<{ ok: boolean; report: DupScanReport }>('/api/dedup/scan', { path, maxFiles }),
  /** KI-2：报告导出直链（`<a download>` 用，避免在桌面 WebView 里 window.open 被拦） */
  dedupExportUrl: (opts: { format?: 'csv' | 'md'; range?: string; scope?: string } = {}) => {
    const q = new URLSearchParams()
    q.set('format', opts.format ?? 'csv')
    if (opts.range && opts.range !== 'all') q.set('range', opts.range)
    if (opts.scope) q.set('scope', opts.scope)
    return `/api/dedup/export?${q.toString()}`
  },
  dedupResolve: (keepAssetId: number, removeIds: number[]) =>
    post<{ ok: boolean; removed: number }>('/api/dedup/resolve', { keepAssetId, removeIds }),
  dedupTrash: (limit = 200) => jfetch<{ items: AssetView[] }>(`/api/dedup/trash?limit=${limit}`),
  dedupRestore: (ids: number[]) =>
    post<{ ok: boolean; restored: number }>('/api/dedup/restore', { ids }),
  /**
   * KI-11 彻底删除（回收站 → 永久删除，**不可撤销**）。
   * 只接受回收站内（deleted=1）的 id：其余进 `skipped`（不静默放过）。
   * 能力位 `features.physicalDelete` 为 false（网页容器 / 库根只读）时后端回 400 + 人话。
   */
  dedupPurge: (ids: number[]) =>
    post<{
      ok: boolean
      /** 真的被彻底清掉的张数 */
      purged: number
      /** 被拒绝的张数（不在回收站 / 已不存在） */
      skipped: number
      purgedIds: number[]
      skippedIds: number[]
      /** 删掉的库行数 */
      rows: number
      /** 删掉的向量行数 */
      vectors: number
      /** 删掉的缩略图文件数（256/1024 两档） */
      thumbs: number
      /** 删掉的磁盘文件数（缩略图 + copyin 库内原图） */
      filesDeleted: number
      /** 真实释放的字节数 */
      freedBytes: number
    }>('/api/dedup/purge', { ids }),

  // ---- STORAGE-MIGRATE-1：一键迁入库（reference → copyin，DT6 后端 / DT7 前端） ----
  /**
   * 启动一批迁移：202 + `{batchId, state:"running", total}`，实际搬运在后台跑。
   *
   * 同一时刻只允许一批（另一批在跑 → 409）。`ids` 里**已经是 copyin / 失联 / 无指纹**的项
   * 由后端逐条进 `report.skipped[]`（带稳定 reason），前端不必先过滤但最好按 `isMigratable`
   * 只提交能迁的那些（跳过清单才有意义）。
   */
  migrateToLibrary: (req: MigrateRequest) =>
    post<MigrateDispatch>('/api/library/migrate-to-library', req),
  /** 进度快照；不传 batchId = 最近一次任务，没有任务 → `{state:"idle"}`。 */
  migrateToLibraryStatus: (batchId?: string) =>
    jfetch<MigrateStatus>(
      `/api/library/migrate-to-library/status${
        batchId ? `?batchId=${encodeURIComponent(batchId)}` : ''
      }`,
    ),

  // ---- 应用信息（导出默认目录等） ----
  appInfo: () => jfetch<AppInfo>('/api/app-info'),

  // ---- WEBCAP：形态能力开关（flag 由服务端算；UI 只照着灰置，不自己判形态） ----
  capabilities: () => jfetch<Capabilities>('/api/capabilities'),

  /**
   * KI-9/KI-10：在系统文件管理器中打开/选中（桌面形态）。
   *
   * 服务端白名单 + 形态门控（见 `crates/app-web/src/os_api.rs`）：库根/导入源/导出目标/回收站原路径外
   * → 400 `path_not_allowed`；容器形态 → 400 `feature_disabled`；路径不存在/盘已卸 → 409 `path_missing`。
   * 失败抛 `ApiError`（`code` 带去因，人话由 `<PathLink>` 统一映射，页面不要再各写一套）。
   */
  osReveal: (path: string, mode: 'reveal' | 'open' = 'reveal') =>
    post<OsRevealResult>('/api/os/reveal', { path, mode }),

  // ---- C6 合规（处理插件管理 + 低置信度复核队列 + 人工改判 + 三态统计） ----
  complianceStatus: () => jfetch<ComplianceStatusView>('/api/compliance/status'),
  complianceQueue: (
    opts: {
      limit?: number
      offset?: number
      onlyUnresolved?: boolean
      lowConfidence?: boolean
      reviewed?: boolean
      threshold?: number
      sort?: 'asset' | 'confidence'
    } = {},
  ) => {
    const p = new URLSearchParams()
    if (opts.limit != null) p.set('limit', String(opts.limit))
    if (opts.offset != null) p.set('offset', String(opts.offset))
    if (opts.onlyUnresolved) p.set('onlyUnresolved', 'true')
    if (opts.lowConfidence != null) p.set('lowConfidence', String(opts.lowConfidence))
    if (opts.reviewed != null) p.set('reviewed', String(opts.reviewed))
    if (opts.threshold != null) p.set('threshold', String(opts.threshold))
    if (opts.sort) p.set('sort', opts.sort)
    return jfetch<ComplianceQueueView>(`/api/compliance/queue?${p}`)
  },
  complianceVerdicts: (assetId: number) =>
    jfetch<ComplianceVerdictsView>(`/api/compliance/verdicts?assetId=${assetId}`),
  /**
   * 批量形态（合规页「按状态浏览」网格）：一次拿整页每张图的判定摘要。
   * 服务端 `assetIds` ≤200，判定口径与单体同一实现（payload 不分叉）。
   */
  complianceVerdictsBatch: (assetIds: number[]) =>
    jfetch<{
      packId: string
      items: {
        assetId: number
        state: ComplianceStateWord
        stateLabel: string
        machineState: ComplianceStateWord
        machineStateLabel: string
        machineConfidence: number | null
        assetHumanReview: HumanReviewView | null
        items: ComplianceVerdictView[]
      }[]
    }>(`/api/compliance/verdicts?assetIds=${assetIds.join(',')}`),
  /**
   * N5b：结果表（`GET /api/compliance/results`）。
   *
   * 服务端按安装记录的 `outputs` + 包内 `resources/result_columns.json` 决定列；
   * 前端**不猜**：`columns=null` 就用默认列，`outputs` 不含 judge 就不画审核列。
   */
  complianceResults: (
    /**
     * N12：`runId` = 按**一次执行**看结果（任务中心用这个）；`jobId` = 单资产那次队列行（诊断用）。
     * 两者不通用：一次执行 = N 个资产队列行。
     */
    opts: { packId?: string; jobId?: number; runId?: number; limit?: number; offset?: number } = {},
  ) => {
    const p = new URLSearchParams()
    if (opts.packId) p.set('packId', opts.packId)
    if (opts.jobId != null) p.set('jobId', String(opts.jobId))
    if (opts.runId != null) p.set('runId', String(opts.runId))
    if (opts.limit != null) p.set('limit', String(opts.limit))
    if (opts.offset != null) p.set('offset', String(opts.offset))
    return jfetch<ComplianceResultsView>(`/api/compliance/results?${p}`)
  },
  /**
   * PROC-RES-2：把选中的处理产物收齐到一个目录（后端机器绝对路径）。
   *
   * 既有 `/api/export/start` 只认库内资产，不认 `compliance/fixes` 下的产物文件 ——
   * 这是产物专用的最小收集入口；返回 `files`（落盘路径）与 `missing`（没过校验的条目）。
   */
  complianceArtifactsExport: (req: {
    items: { path: string; name?: string }[]
    destDir: string
  }) =>
    post<{
      ok: boolean
      total: number
      copied: number
      files: string[]
      missing: { path: string; reason: string; code: string }[]
      dest: string
    }>('/api/compliance/artifacts/export', req),
  /**
   * DECouple-FE（卡 t_ad156948）：**按这次 job 的结果导出子集**（任务中心 L2）。
   * F-4（t_8d8eeeb5）：请求字段已对齐后端 canonical 名 `status`（原 `verdict` 被 serde
   * 静默忽略 → 恒导出 all）；回执按后端真实返回 `{ok, runId, status, selected, report}`
   * 渲染（原按 `{ok, copied, dest, missing}` 读 → undefined 崩溃谎报失败）。
   * 契约：
   *   POST /api/compliance/jobs/{runId}/export
   *   body  { status: 'all' | 'pass' | 'fail' | 'undetermined', destDir: string }
   *   resp  { ok, runId, status, selected, report: ExportReport }
   *
   * 与 F8 图库导出**互不相干**：图库导出只按资产域条件（选择集/筛选/排除拒选），不读判定字段。
   */
  complianceJobExport: (
    runId: number,
    req: { status: 'all' | 'pass' | 'fail' | 'undetermined'; destDir: string },
  ) =>
    post<{
      ok: boolean
      runId: number
      status: string
      selected: number
      report: ExportReport
    }>(`/api/compliance/jobs/${runId}/export`, req),
  /**
   * 复核列表（**扁平**：一个 (asset, rule) 一行 + 资产级改判一行）。
   * `lowConfidence` 缺省：给了 assetId → 该图全部有结论的行；没给 → 只列需要复核的行。
   */
  complianceReview: (
    opts: {
      assetId?: number
      lowConfidence?: boolean
      threshold?: number
      reviewed?: boolean
      sort?: 'asset' | 'confidence'
      limit?: number
      offset?: number
    } = {},
  ) => {
    const p = new URLSearchParams()
    if (opts.assetId != null) p.set('assetId', String(opts.assetId))
    if (opts.lowConfidence != null) p.set('lowConfidence', String(opts.lowConfidence))
    if (opts.threshold != null) p.set('threshold', String(opts.threshold))
    if (opts.reviewed != null) p.set('reviewed', String(opts.reviewed))
    if (opts.sort) p.set('sort', opts.sort)
    if (opts.limit != null) p.set('limit', String(opts.limit))
    if (opts.offset != null) p.set('offset', String(opts.offset))
    return jfetch<ComplianceReviewListView>(`/api/compliance/review?${p}`)
  },
  /** 人工改判（reason 必填；高置信度结论要 force=true，否则 400 review_not_low_confidence） */
  complianceReviewCreate: (req: {
    assetId: number
    /** 缺省 = 资产级改判（整张图一条结论） */
    ruleId?: string | null
    humanStatus: 'pass' | 'fail'
    reason: string
    reviewedBy?: string
    force?: boolean
  }) => post<ComplianceReviewResult>('/api/compliance/review', req),
  /** 撤销改判（回到机器结论；不删行，只落 revokedAt） */
  complianceReviewRevoke: (req: { assetId: number; ruleId?: string | null; reason?: string }) => {
    const p = new URLSearchParams()
    p.set('assetId', String(req.assetId))
    if (req.ruleId) p.set('ruleId', req.ruleId)
    if (req.reason) p.set('reason', req.reason)
    return del<{
      ok: boolean
      revoked: number
      assetId: number
      ruleId: string | null
      assetState: string | null
      assetStateLabel: string | null
      machineState: string | null
      machineStateLabel: string | null
    }>(`/api/compliance/review?${p}`)
  },
  /** 改判历史（含撤销/被覆盖；审计 + 改判前后对照） */
  complianceReviewHistory: (opts: { assetId?: number; limit?: number } = {}) => {
    const p = new URLSearchParams()
    if (opts.assetId != null) p.set('assetId', String(opts.assetId))
    if (opts.limit != null) p.set('limit', String(opts.limit))
    return jfetch<{ packId: string; total: number; items: ComplianceReviewHistoryItem[] }>(
      `/api/compliance/review/history?${p}`,
    )
  },
  /** 旧口径误判反馈（只读；写入口已并入改判，界面不再调用） */
  complianceFeedbackList: (opts: { assetId?: number; ruleId?: string; limit?: number } = {}) => {
    const p = new URLSearchParams()
    if (opts.assetId != null) p.set('assetId', String(opts.assetId))
    if (opts.ruleId) p.set('ruleId', opts.ruleId)
    if (opts.limit != null) p.set('limit', String(opts.limit))
    return jfetch<{
      total: number
      packId: string
      items: (ComplianceFeedbackView & { assetId: number; ruleId: string })[]
    }>(`/api/compliance/feedback?${p}`)
  },
  /** 反馈 CSV（浏览器直下；含 BOM，Excel 直开） */
  complianceFeedbackCsvUrl: () => '/api/compliance/feedback/export',
  compliancePacks: () =>
    jfetch<{
      /** IP-K08：已安装的处理插件（版本 / 运行态 / 签名 / 是否内置） */
      plugins?: ComplianceInstalledPluginPack[]
      /** IP-K08：安装期签名策略（配了信任公钥 = 强制验签） */
      signaturePolicy?: ComplianceSignaturePolicyView
      /** IP-K08：本部署实际授权的能力集合（编译期全集 ∩ settings 子集） */
      hostCapabilities?: string[]
      /** IP-K16：判定实际拉起的插件态（与 /status.pluginState 同源） */
      pluginState?: CompliancePluginState
    }>('/api/compliance/packs'),
  /** IP-K08：插件包详情（manifest + 规则文档；`probe` = 顺带拉一次 pack.info，会起子进程） */
  // `probe` 只在要拉起 `pack.info` 时才传：后端把 query 当严格布尔解析，`?probe=0` 会直接 400
  // （N4 手验踩过：详情页一直报 400 Bad Request，界面看起来像「包坏了」）
  compliancePluginPackDetail: (id: string, probe = false) =>
    jfetch<CompliancePluginPackDetail>(
      `/api/compliance/packs/${encodeURIComponent(id)}${probe ? '?probe=1' : ''}`,
    ),
  /**
   * IP-K08：安装 `.dampack`（zip: manifest.json + plugin/ + resources/ + README.md）。
   * 两种投递：`path`（服务端可见路径，桌面/容器同机）或 `archiveBase64`（浏览器上传）。
   * `activate` 缺省 = 库里还没有该包的生效版本时才切 current（补装旧版本不抢指针）。
   */
  compliancePackInstall: (req: {
    path?: string
    archiveBase64?: string
    filename?: string
    activate?: boolean
  }) => post<CompliancePackInstallResult>('/api/compliance/packs/install', req),
  /** IP-K08：切 current 指针（缺省切到最新已装版本） */
  compliancePackActivateVersion: (id: string, version?: string) =>
    post<CompliancePackSwitchResult>(
      `/api/compliance/packs/${encodeURIComponent(id)}/activate`,
      version ? { version } : {},
    ),
  /** IP-K08：回滚（缺省切到上一个已装版本；旧版本目录保留） */
  compliancePackRollback: (id: string, version?: string) =>
    post<CompliancePackSwitchResult>(
      `/api/compliance/packs/${encodeURIComponent(id)}/rollback`,
      version ? { version } : {},
    ),
  /** IP-K08：卸载（内置包 403；当前生效版本单独卸 409） */
  compliancePackUninstall: (id: string, version?: string) =>
    del<CompliancePackUninstallResult>(
      `/api/compliance/packs/${encodeURIComponent(id)}${version ? `?version=${encodeURIComponent(version)}` : ''}`,
    ),
  /** JUDGE-ENTRY：产品级批量判定（缺省 = 当前处理插件 + 未判定资产 + 逐图与组级一起跑） */
  complianceJudge: (req: {
    assetIds?: number[]
    all?: boolean
    packId?: string
    scope?: 'asset' | 'group' | 'both'
    limit?: number
    workers?: number
    /** N1：引用一条规格（`plugin_spec.specId`）—— 规则子集/阈值/参数 diff 都从规格来 */
    specId?: number
  }) => post<ComplianceJudgeStartView>('/api/compliance/judge', req),
  /**
   * N12：规格列表（`GET /api/specs`，按能力过滤）。「检查」用无动作链的规格，
   * 「处理」用有动作链的规格 —— 分类见 `lib/specs.ts`（不从插件目录猜）。
   */
  /**
   * N12：**执行入口**（`POST /api/jobs`，N1 定名：设计口径的单入口，接受 `specId`）。
   * 「检查」「处理」都从这里进去 —— 用户不区分「任务类型」，宿主按规格分流。
   * `all: true` = 全库（后端 JudgeReq.all；只允许判定类界面传，处理类不给一键改全库）。
   */
  startJob: (req: {
    specId: number
    assetIds?: number[]
    all?: boolean
    options?: Record<string, unknown>
  }) => post<ComplianceJudgeStartView>('/api/jobs', req),
  complianceJudgeStatus: () =>
    jfetch<ComplianceJudgeStatusView>('/api/compliance/judge/status'),
  complianceJudgeCancel: () =>
    post<{ ok: boolean; cancelling: boolean }>('/api/compliance/judge/cancel', {}),
  // CMP-JOB-01：一次批量审核 = 一个独立 job（执行列表 / 单次明细 / 正在跑的那一次）
  complianceJobs: (opts: { limit?: number; offset?: number } = {}) => {
    const p = new URLSearchParams()
    if (opts.limit != null) p.set('limit', String(opts.limit))
    if (opts.offset != null) p.set('offset', String(opts.offset))
    const qs = p.toString()
    return jfetch<ComplianceJobsView>(`/api/compliance/jobs${qs ? `?${qs}` : ''}`)
  },
  complianceJob: (runId: number, opts: { limit?: number; offset?: number } = {}) => {
    const p = new URLSearchParams()
    if (opts.limit != null) p.set('limit', String(opts.limit))
    if (opts.offset != null) p.set('offset', String(opts.offset))
    const qs = p.toString()
    return jfetch<ComplianceJobDetailView>(
      `/api/compliance/jobs/${runId}${qs ? `?${qs}` : ''}`,
    )
  },
  complianceJobCurrent: () => jfetch<ComplianceJobCurrentView>('/api/compliance/jobs/current'),

  // ---- N2/N1：插件参数 schema + 规格（K07-I / t_1cde30f2）----
  /**
   * 插件**参数声明**（只读包内 `params.schema.json`，不启子进程 → 廉价接口）。
   * 404 `plugin_not_found` = 包不在（界面给「装好后可用」，不是「这插件没参数」）。
   */
  pluginParamsSchema: (id: string) =>
    jfetch<PluginParamsSchemaView>(`/api/plugins/${encodeURIComponent(id)}/schema`),
  /** 列规格（不传 plugin = 全部；老库首次访问会惰性补默认规格） */
  specs: (plugin?: string) =>
    jfetch<ComplianceSpecList>(`/api/specs${plugin ? `?plugin=${encodeURIComponent(plugin)}` : ''}`),
  spec: (specId: number) => jfetch<ComplianceSpecView>(`/api/specs/${specId}`),
  specCreate: (req: ComplianceSpecCreateReq) => post<ComplianceSpecView>('/api/specs', req),
  specUpdate: (specId: number, req: ComplianceSpecPatchReq) =>
    jfetch<ComplianceSpecView>(`/api/specs/${specId}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(req),
    }),
  /** 只删配置行：历史判定、产物、插件包一律不动 */
  specDelete: (specId: number) =>
    jfetch<{ specId: number; deleted: boolean }>(`/api/specs/${specId}`, { method: 'DELETE' }),
  /** 「先跑 1 张」：用这条规格对一张图起一次判定 run（不是第二条执行链路，走 judge） */
  specTry: (specId: number, assetId?: number) =>
    post<ComplianceJudgeStartView>(`/api/specs/${specId}/try`, assetId != null ? { asset_id: assetId } : {}),
  /** 显式重跑旧行业包 → 默认规格的幂等迁移（返回回读校验报告） */
  specsMigrate: () => post<Record<string, unknown>>('/api/specs/migrate', {}),

  // ---- 媒体 ----
  thumbUrl: (assetId: number, size: 256 | 1024 = 256) => `/api/media/thumb/${assetId}?size=${size}`,
  originalUrl: (assetId: number) => `/api/media/original/${assetId}`,

  // ---- W3 搜索 ----
  /** minScore：相关性下限（cos），低于阈值不返回；不传用后端默认 0.085，传 0 = 不过滤 */
  searchText: (query: string, k: number, filter?: SearchFilterReq, minScore?: number) =>
    post<SearchResult>('/api/search/text', { query, k, filter, minScore }),
  /**
   * 库内「找相似」。minScore 缺省 = 后端按模型标定的相关性下限（siglip2 0.72；未标定模型不过滤），
   * 显式传 0 = 关闭过滤（旧行为：返回一堆无关图）。
   * mode 显式透传（KI-14：档位来自检查器 split button，两档请求体可对账 mode=semantic|visual_precise）。
   */
  searchSimilar: (
    assetId: number,
    k: number,
    filter?: SearchFilterReq,
    minScore?: number,
    mode?: SimilarMode,
  ) => post<SearchResult>('/api/search/similar', { assetId, k, filter, minScore, mode }),
  /**
   * 库内「找相似」的精准匹配（KI-5）：同图变体 / 近似重复。
   * 后端用判重塔 rerank + 0.90/0.98 分带，**不接受 minScore**（传了会 400）；
   * 缺判重向量时回 200 + 空结果 + `missingDedupVectors`/`queryMissingDedupVector` 标记。
   */
  searchSimilarPrecise: (assetId: number, k: number, filter?: SearchFilterReq) =>
    post<PreciseResult>('/api/search/similar', { assetId, k, filter, mode: 'visual_precise' }),
  /**
   * 上传图片搜索：原始字节 body（Content-Type 取自文件）；k / filter / minScore 走 query
   * （后端 by_image 已接这三项，见 lib/searchQuery.ts 的 searchImageQuery）。
   */
  searchImage: (file: File, opts?: { k?: number; filter?: SearchFilterReq; minScore?: number }) =>
    jfetch<SearchResult>('/api/search/image' + searchImageQuery(opts), {
      method: 'POST',
      headers: { 'content-type': file.type || 'image/jpeg' },
      body: file,
    }),
  searchStatus: () => jfetch<SearchStatus>('/api/search/status'),

  // ---- W3 推理设置（embed provider 可插拔） ----
  inference: () => jfetch<InferenceResponse>('/api/settings/inference'),
  saveInference: (s: InferenceSettingsDto) =>
    jfetch<{ ok: boolean; settings: InferenceSettingsDto }>('/api/settings/inference', {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(s),
    }),
  testEmbed: () =>
    post<{ ok: boolean; modelKey: string; dims: number; endpointModel: string; baseUrl: string }>(
      '/api/settings/embed/test',
      {},
    ),

  // ---- W3 嵌入队列（建向量索引） ----
  /**
   * 启动嵌入任务。mode: pending（增量）/ all（全库重嵌）/ backfill（只补从未跑过的存量）。
   * scope: semantic（默认，语义塔）/ dedup（判重塔，模型写死）/ all（两塔）。
   * 去重塔无界面入口（KI-2/6），此处只做**接口透传**：维护/诊断脚本或后续 KI-2 入口复用。
   */
  embedStart: (mode: 'pending' | 'all' | 'backfill' = 'pending', scope?: 'semantic' | 'dedup' | 'all') =>
    post<{
      ok: boolean
      queued: number
      queuedByTower?: Record<string, number>
      mode: string
      scope?: string
      model: string
      models?: Record<string, string>
      towerErrors?: { scope: string; error: string }[]
    }>('/api/embed/start', scope ? { mode, scope } : { mode }),
  embedCancel: () => post<{ ok: boolean }>('/api/embed/cancel', {}),
  embedStatus: () => jfetch<EmbedStatus>('/api/embed/status'),
  embedFailed: () =>
    jfetch<{ items: { jobId: number; assetId: number; model: string; attempts: number; lastError: string }[] }>(
      '/api/embed/failed',
    ),
  embedRetryFailed: () => post<{ ok: boolean; reset: number }>('/api/embed/retry-failed', {}),

  // ---- W4 打标设置（D9/R7：预设表 / Ollama 扫描 / 连接测试） ----
  tagPresets: () =>
    jfetch<{ presets: TagProviderPresetDto[] }>('/api/settings/tag/presets'),
  tagScan: (baseUrl: string) =>
    jfetch<{ models: string[] }>(
      `/api/settings/tag/scan?base_url=${encodeURIComponent(baseUrl)}`,
    ),
  tagTest: (tag: TagSettingsDto) =>
    post<TagTestResultView>('/api/settings/tag/test', tag),

  // ---- W4 模板打标队列 ----
  tagStart: (mode: 'pending' | 'all', limit?: number) =>
    post<{ ok: boolean; queued: number; mode: string; limit: number }>('/api/tag/start', {
      mode,
      limit: limit ?? 0,
    }),
  tagCancel: () => post<{ ok: boolean }>('/api/tag/cancel', {}),
  tagStatus: () => jfetch<TagStatusView>('/api/tag/status'),
  tagFailed: () => jfetch<{ items: TagFailedItemView[] }>('/api/tag/failed'),
  /** class 缺省 = 全部失败项；truncated = 只重试「截断」类。重置后后端直接续跑。 */
  tagRetryFailed: (cls?: 'truncated' | 'other') =>
    post<{ ok: boolean; reset: number; started: boolean }>(
      `/api/tag/retry-failed${cls ? `?class=${cls}` : ''}`,
      {},
    ),

  // ---- OCR-E 文字识别队列 ----
  ocrStart: (mode: 'pending' | 'all', limit = 0) =>
    post<{ enqueued: number; mode: string; limit: number }>('/api/ocr/start', { mode, limit }),
  ocrCancel: () => post<{ cancelling: boolean }>('/api/ocr/cancel', {}),
  ocrStatus: () => jfetch<OcrStatusView>('/api/ocr/status'),
  ocrFailed: () => jfetch<{ items: TagFailedItemView[] }>('/api/ocr/failed'),
  ocrRetryFailed: () => post<{ requeued: number }>('/api/ocr/retry-failed', {}),
  ocrAssetText: (assetId: number) =>
    jfetch<{ assetId: number; ocrText: string }>(`/api/ocr/asset?assetId=${assetId}`),

  // ---- 模型获取入口（卡 t_6e8888ca：lama-fp32 等未内置模型的下载） ----
  /** manifest 落盘状态（缺哪件、多大、引擎实际读取的绝对路径） */
  models: () => jfetch<ModelsView>('/api/models'),
  /**
   * 触发单飞下载。baseUrl 覆盖设置里的「模型下载源」（镜像根 / `file://` 内网目录）；
   * 已在盘的会跳过（force 才重下）。409 = 已有下载在跑 / 分发源未配置 / 本构建无本地运行时。
   */
  modelDownload: (name: string, baseUrl?: string, force = false) =>
    post<{ ok: boolean; model: string; baseUrl: string; root: string; totalBytes: number; files: number }>(
      `/api/models/${encodeURIComponent(name)}/download`,
      baseUrl ? { baseUrl, force } : { force },
    ),
  modelDownloadStatus: () => jfetch<ModelDownloadStatus>('/api/models/download/status'),

  // ---- W4 自定义打标任务 ----
  tagTasks: () => jfetch<{ items: CustomTagTaskDto[] }>('/api/tag/tasks'),
  tagTaskCreate: (name: string, prompt: string, enabled = true) =>
    post<{ ok: boolean; id: number }>('/api/tag/tasks', { name, prompt, enabled }),
  tagTaskUpdate: (id: number, name: string, prompt: string, enabled: boolean) =>
    jfetch<{ ok: boolean }>(`/api/tag/tasks/${id}`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ name, prompt, enabled }),
    }),
  tagTaskDelete: (id: number, purge: boolean) =>
    jfetch<{ ok: boolean; deleted: number; purgedTags: number }>(
      `/api/tag/tasks/${id}?purge=${purge}`,
      { method: 'DELETE' },
    ),
  tagTaskRun: (id: number, mode: 'all' | 'resume' = 'all', limit?: number) =>
    post<{ ok: boolean; queued: number; mode: string; limit: number }>(
      `/api/tag/tasks/${id}/run`,
      { mode, limit: limit ?? 0 },
    ),
  tagCustomCancel: () => post<{ ok: boolean }>('/api/tag/custom/cancel', {}),
  tagCustomFailed: () =>
    jfetch<{ items: CustomTagFailedItemView[] }>('/api/tag/custom/failed'),
  tagCustomRetryFailed: () =>
    post<{ ok: boolean; reset: number; started: boolean }>(
      '/api/tag/custom/retry-failed',
      {},
    ),

  // ---- E5 打标确认流（两段式：AI 结果先暂存，确认才入库） ----
  tagReview: (limit = 200, offset = 0) =>
    jfetch<TagReviewList>(`/api/tag/review?limit=${limit}&offset=${offset}`),
  tagReviewUpdate: (
    assetId: number,
    body: {
      deletes?: number[]
      updates?: { id: number; name?: string; value?: string }[]
      adds?: { kind: string; name: string; value?: string }[]
    },
  ) => put<{ ok: boolean }>(`/api/tag/review/items/${assetId}`, body),
  tagReviewCommit: (assetIds: number[], all = false) =>
    post<{ ok: boolean; report: { assets: number; tags: number; fields: number } }>(
      '/api/tag/review/commit',
      { assetIds, all },
    ),
  tagReviewDiscard: (assetIds: number[], all = false) =>
    post<{ ok: boolean; removed: number }>('/api/tag/review/discard', { assetIds, all }),
}

/** 字节数 → 人类可读（W1 mock.formatSize 的 KB 版本替换为真实字节） */
export function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(0)} KB`
  return `${bytes} B`
}

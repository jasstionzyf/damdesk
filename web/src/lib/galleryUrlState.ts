import type { FilterChip, LibraryFilter } from '../components/LibraryTree'
import type { FolderQuery } from './smartFolders'
import type { ExportFilterPayload } from './api'
import { withParams, type Params } from './urlState'
import { tGlobal } from '../i18n'

/**
 * 图库页的 URL 状态契约（URL-STATE-1 / 卡 task；D-CHAIN-1 / 卡 task 合流到 main）。
 *
 * 为什么单独成文件：页面里的「URL → LibraryFilter」和「LibraryFilter → URL patch」是纯函数，
 * 抽出来才能在 node 环境单测（本仓 vitest 不带 DOM 兜底），也把键名口径固定在一处 ——
 * 页面里不许再各自拼字符串。
 *
 * 键名口径：
 *  - 目录写 `dir`（历史版本与验收脚本 `scripts/ui_chain_check.js` 都用 `dir`，老链接继续可用）；
 *    `path` 只读别名，认但不写（冻结契约里曾写作 path）。
 *  - 标签写 `tags`；`tag` 只读别名（历史链接）。
 *  - 未打标 `untagged`；仅失联 `missing`；仅无法预览 `broken`；
 *    画幅 `aspect`；相机 `camera`；拍摄区间 `takenFrom`/`takenTo`；
 *    服务端排序 `sort`（importedAt | takenAt）。
 *  - `rating` 是 KI-11 之前的评分下限键：**只解析不消费**（旧深链 / 旧偏好打开不报错，
 *    静默忽略；`galleryClearPatch` 仍会把它从 URL 清掉），任何界面都不再写入。
 */
export const GALLERY_URL_DEFAULTS = {
  q: '',
  tab: 'all' as 'all' | 'recent',
  dir: '',
  /** 只读别名（历史/冻结契约写法），不参与写入 */
  path: '',
  tags: '',
  /** 只读别名（历史写法），不参与写入 */
  tag: '',
  /** KI-11 前的评分下限键：旧深链解析兼容（静默忽略），不再写入 */
  rating: 0,
  untagged: false,
  missing: false,
  broken: false,
  aspect: '',
  camera: '',
  takenFrom: 0,
  takenTo: 0,
  sort: '',
  /**
   * KI-16 #7（卡 task）：来源智能文件夹 id（`?sf=<id>`）。
   * 条件本身仍然落在 dir/tags/untagged/missing 上 —— 智能文件夹只是「绑定了哪一组条件」的标签，
   * 图库据此在筛选横幅上显示「智能文件夹：<名字> ×」（可一键解除绑定）。
   */
  sf: 0,
  open: '',
  ids: [] as number[],
}

export type GalleryUrlState = typeof GALLERY_URL_DEFAULTS

/** URL 状态 → 服务端/侧栏用的 LibraryFilter（唯一入口；页面不要再自己映射） */
export function galleryFilterOf(v: GalleryUrlState): LibraryFilter {
  const f: LibraryFilter = { view: v.tab === 'recent' ? 'recent' : 'all' }
  if (v.untagged) f.untagged = true
  const dir = v.dir || v.path
  if (dir) f.dir = dir
  const tags = v.tags || v.tag
  if (tags) f.tag = tags.split(',')[0]
  // v.rating（旧评分深链）静默忽略 —— KI-11：评分不再是产品能力（galleryUrlState 兼容解析的唯一落点）
  if (v.missing) f.missing = 1
  if (v.broken) f.broken = 1
  if (v.aspect === 'landscape' || v.aspect === 'portrait' || v.aspect === 'square') {
    f.aspect = v.aspect
  }
  if (v.camera) f.cameraModel = v.camera
  if (v.takenFrom > 0) f.takenFrom = v.takenFrom
  if (v.takenTo > 0) f.takenTo = v.takenTo
  if (v.sort === 'importedAt' || v.sort === 'takenAt') f.sort = v.sort
  return f
}

/**
 * LibraryFilter → URL 增量（写全量键：等于默认值的键由 urlState 自动从 URL 移除）。
 * 侧栏目录树 / EXIF 过滤条都经这一个函数落到 URL。
 */
export function galleryUrlPatchOf(f: LibraryFilter): Partial<GalleryUrlState> {
  return {
    tab: f.view === 'recent' ? 'recent' : 'all',
    dir: f.dir ?? '',
    tags: f.tag ?? '',
    untagged: !!f.untagged,
    missing: !!f.missing,
    broken: !!f.broken,
    aspect: f.aspect ?? '',
    camera: f.cameraModel ?? '',
    takenFrom: f.takenFrom ?? 0,
    takenTo: f.takenTo ?? 0,
    sort: f.sort ?? '',
    sf: 0,
  }
}

/**
 * KI-16 #7（卡 task）：智能文件夹条件 → 图库 URL 增量。
 *
 * 智能文件夹不再自成浏览面：点侧栏 = 图库带上保存的条件（`sf=<id>` 只是「来源」标记）。
 * 只映射图库 URL 现有维度（`dir`/`tags`/`untagged`/`missing`）；`colorLabel`/`mediaType`/`ext`
 * 在图库筛选契约里没有对应键 —— 不在这里新造第二套筛选参数，由
 * `smartFolderDroppedDimsOf` 明示「哪些保存维度没能带过来」（不静默丢条件）。
 */
export function smartFolderUrlPatchOf(q: FolderQuery, folderId: number): Partial<GalleryUrlState> {
  const patch: Partial<GalleryUrlState> = { sf: folderId > 0 ? folderId : 0 }
  if (q.dirPrefix) patch.dir = q.dirPrefix
  if (q.tag) patch.tags = q.tag
  if (q.untagged) patch.untagged = true
  if (q.missing === 1) patch.missing = true
  return patch
}

/** 智能文件夹 → 图库地址（侧栏点击 / `/smart/:id` 重定向共用；只写非默认键） */
export function smartFolderHref(folderId: number, q: FolderQuery): string {
  const patch: Params = { sf: folderId }
  if (q.dirPrefix) patch.dir = q.dirPrefix
  if (q.tag) patch.tags = q.tag
  if (q.untagged) patch.untagged = true
  if (q.missing === 1) patch.missing = true
  return withParams('/', patch)
}

/**
 * 智能文件夹条件 → `/api/library/assets` 查询（**与图库 `queryOf(filter)` 同一口径**）。
 * 保存时用它取 `total` 记下来，图库应用后再对账（不等 → 芯片标「条件已变」）。
 */
export interface SmartFolderAssetQuery {
  dirPrefix?: string
  tag?: string
  untagged?: true
  missing?: 1
}

export function smartFolderAssetQuery(q: FolderQuery): SmartFolderAssetQuery {
  const out: SmartFolderAssetQuery = {}
  if (q.dirPrefix) out.dirPrefix = q.dirPrefix
  if (q.tag) out.tag = q.tag
  if (q.untagged) out.untagged = true
  if (q.missing === 1) out.missing = 1
  return out
}

/** 保存了、但图库筛选契约表达不了的维度（人话）——芯片上明示，避免「条件被静默吞掉」 */
export function smartFolderDroppedDimsOf(q: FolderQuery): string[] {
  const out: string[] = []
  if (q.colorLabel) out.push(tGlobal('颜色'))
  if (q.mediaType) out.push(tGlobal('类型'))
  if (q.ext) out.push(tGlobal('扩展名'))
  return out
}

/**
 * LibraryFilter → 导出「当前筛选结果」的查询条件（卡 task：与图库 queryOf 同一映射，
 * 抽出来单一实现，页面不许再手抄一份 —— 两份映射漂移就是 DT4 的形状）。
 *
 * `q`（关键词，C7 / 卡 task）：它是页面态（不进 LibraryFilter、不持久化），但**必须**
 * 随「当前筛选结果」一起导出 —— 否则图库筛出 2 张、导出却是整库（最坏的数据事故）。
 * 后端 `ImportFilter` 对同名键做了 serde alias，两边键名同一个 `q`。
 *
 * `view === 'recent'` 返回 null：最近视图是**页面视图**（近 24 张），不是筛选条件；
 * 后端 ImportFilter 也没有 recent 字段，硬塞会被 serde 静默忽略 → 导出悄悄变整库。
 */
export function exportFilterOf(f: LibraryFilter, q?: string): ExportFilterPayload | null {
  if (f.view === 'recent') return null
  const qq = (q ?? '').trim()
  const out: ExportFilterPayload = {}
  if (qq) out.q = qq
  if (f.untagged) out.untagged = true
  if (f.dir) out.dirPrefix = f.dir
  if (f.tag) out.tag = f.tag
  if (f.missing) out.missing = f.missing
  if (f.broken === 1) out.broken = 1
  if (f.aspect) out.aspect = f.aspect
  if (f.cameraModel) out.cameraModel = f.cameraModel
  if (f.takenFrom != null) out.takenFrom = f.takenFrom
  if (f.takenTo != null) out.takenTo = f.takenTo
  if (f.sort) out.sort = f.sort
  // 空条件 = 整库：显式返回 {}（后端 build_spec 要求 filter 与 assetIds 二选一，
  // 传 undefined 会被判「导出来源非法」——task UI 走查实测）。
  return out
}

/** 芯片维度 → URL 键（命名不完全一致：tag→tags / cameraModel→camera） */
const CHIP_KEY: Partial<Record<FilterChip['field'], keyof GalleryUrlState>> = {
  /** KI-16 #7：移除「智能文件夹：<名字>」芯片 = 解除绑定（其余条件作为普通芯片保留） */
  sf: 'sf',
  dir: 'dir',
  tag: 'tags',
  untagged: 'untagged',
  missing: 'missing',
  broken: 'broken',
  aspect: 'aspect',
  cameraModel: 'camera',
}

/**
 * 芯片一键移除 → 只把那一个维度写回默认值（其余维度保留）。
 * `query` 不在这里：关键词是 `q` 键，由调用方单独清。
 */
export function galleryChipPatchOf(field: FilterChip['field']): Partial<GalleryUrlState> {
  if (field === 'takenRange') return { takenFrom: 0, takenTo: 0 }
  const key = CHIP_KEY[field]
  if (!key) return {}
  return { [key]: GALLERY_URL_DEFAULTS[key] } as Partial<GalleryUrlState>
}

/** 清空全部筛选（来源提示横幅的「清除」用；不动 tab 以外的浮层/选择集；顺带清掉旧评分键） */
export function galleryClearPatch(): Partial<GalleryUrlState> {
  return {
    q: '',
    dir: '',
    path: '',
    tags: '',
    tag: '',
    rating: 0,
    untagged: false,
    missing: false,
    broken: false,
    aspect: '',
    camera: '',
    takenFrom: 0,
    takenTo: 0,
    sort: '',
  }
}

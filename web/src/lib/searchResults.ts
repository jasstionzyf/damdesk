import type { AssetView, SearchHit } from './api'

/**
 * 搜索结果 → 图库视图（卡 task）。
 *
 * 搜索页不再自己画一套结果卡：命中先落成「最小可交互的 AssetView」占位，
 * 随后用 /api/library/assets?ids= 回读真值补全（文件名/尺寸/评分/拒选态等），
 * 顺序恒等于命中顺序（相似度降序）。回读失败时保留占位 —— 卡片仍可选中/批量操作，
 * 只是少了元数据（不静默丢结果）。
 */

/** 命中顺序（相似度降序的 assetId 数组）—— 选择模型的范围选/全选都按它走 */
export function hitOrder(hits: { assetId: number }[]): number[] {
  return hits.map((h) => h.assetId)
}

/** 命中的最小 AssetView：只填卡片渲染必需字段，其余给中性默认值 */
export function hitToAsset(hit: SearchHit): AssetView {
  return {
    assetId: hit.assetId,
    mediaType: 'image',
    absPath: hit.thumbPath ?? '',
    storeMode: '',
    missing: 0,
    fileName: hit.fileName,
    fileExt: (hit.fileName.split('.').pop() ?? '').toLowerCase(),
    fileSz: 0,
    mtime: 0,
    fileHash: null,
    titleCh: hit.titleCh ?? null,
    titleEn: null,
    description: null,
    source: '',
    parentAssetId: null,
    searchable: 1,
    deleted: 0,
    approvalStatus: 'pending',
    starRating: 0,
    colorLabel: null,
    cloudAssetId: null,
    importedAt: 0,
    embStatus: 'done',
    embModel: null,
    dedupStatus: 'none',
    dedupModel: null,
    thumbPath: hit.thumbPath ?? null,
    width: hit.width ?? null,
    height: hit.height ?? null,
    pHash: null,
    takenAt: null,
    cameraMake: null,
    cameraModel: null,
    lensModel: null,
    iso: null,
    focalLenMm: null,
    aperture: null,
    shutterText: null,
    orientation: null,
    aspectRatio: null,
    aspectClass: null,
    gpsLat: null,
    gpsLon: null,
    gpsSource: null,
    exif: null,
  }
}

/**
 * 回读结果按命中顺序重排：命中里有的 id 用真值，回读缺失的仍用占位。
 * （后端按 importedAt 返回，不能直接当视图顺序用。）
 */
export function orderByHits(hits: SearchHit[], full: AssetView[]): AssetView[] {
  const byId = new Map(full.map((a) => [a.assetId, a]))
  return hits.map((h) => byId.get(h.assetId) ?? hitToAsset(h))
}

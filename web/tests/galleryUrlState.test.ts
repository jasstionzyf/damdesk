/**
 * D-CHAIN-1（卡 t_319596b3）：图库 URL 状态纯函数单测。
 *
 * 覆盖三件事：
 *  1. URL → LibraryFilter：目录/标签的别名兼容（dir/path、tags/tag）、评分上限、三态合规、
 *     EXIF 维度、服务端排序；
 *  2. LibraryFilter → URL patch 的往返一致（深链/刷新复现同一屏的前提）；
 *  3. 芯片移除只动一个维度、清除只清筛选不动 tab 以外的浮层/选择集语义（patch 里不含 open/ids）。
 *
 * 这些断言同时是 URL 契约的书面口径：改键名必须同步改这里，别在页面里私自加键。
 */
import { describe, expect, it } from 'vitest'
import type { LibraryFilter } from '../src/components/LibraryTree'
import {
  GALLERY_URL_DEFAULTS,
  exportFilterOf,
  galleryChipPatchOf,
  galleryClearPatch,
  galleryFilterOf,
  galleryUrlPatchOf,
} from '../src/lib/galleryUrlState'

const noFilter = { view: 'all' as const }

describe('galleryFilterOf：URL → LibraryFilter', () => {
  it('空 URL → 默认视图 all（不产生任何维度）', () => {
    expect(galleryFilterOf(GALLERY_URL_DEFAULTS)).toEqual(noFilter)
  })

  it('目录键写 dir；path 是只读别名（历史/冻结契约链接继续可用），dir 优先', () => {
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, dir: '/test-fixture/a' })).toEqual({
      view: 'all',
      dir: '/test-fixture/a',
    })
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, path: '/test-fixture/b' })).toEqual({
      view: 'all',
      dir: '/test-fixture/b',
    })
    expect(
      galleryFilterOf({ ...GALLERY_URL_DEFAULTS, dir: '/test-fixture/a', path: '/test-fixture/b' }).dir,
    ).toBe('/test-fixture/a')
  })

  it('标签取首个（逗号分隔多值）；tag 是只读别名', () => {
    expect(
      galleryFilterOf({ ...GALLERY_URL_DEFAULTS, tags: '风景,人像' }).tag,
    ).toBe('风景')
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, tag: '海边' }).tag).toBe('海边')
    expect(
      galleryFilterOf({ ...GALLERY_URL_DEFAULTS, tags: '风景', tag: '海边' }).tag,
    ).toBe('风景')
  })

  it('KI-11：rating 键只解析不消费（旧深链静默忽略，不进 LibraryFilter）', () => {
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, rating: 0 })).not.toHaveProperty('minRating')
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, rating: 4 })).not.toHaveProperty('minRating')
  })

  it('布尔维度：untagged / missing(1) / broken(1)', () => {
    expect(
      galleryFilterOf({ ...GALLERY_URL_DEFAULTS, untagged: true, missing: true, broken: true }),
    ).toEqual({ view: 'all', untagged: true, missing: 1, broken: 1 })
  })

  it('DECouple-FE：判定维度不再进图库筛选（旧深链 `?compliance=` 静默忽略）', () => {
    // 旧链接带 compliance 也不该复活成筛选条件：图库是纯资产域，零判定概念
    expect(
      galleryFilterOf({ ...GALLERY_URL_DEFAULTS, compliance: 'fail' } as never),
    ).toEqual({ view: 'all' })
  })

  it('EXIF 维度：画幅白名单、相机精确、拍摄区间透传', () => {
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, aspect: 'portrait' }).aspect).toBe('portrait')
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, aspect: 'bogus' }).aspect).toBeUndefined()
    expect(
      galleryFilterOf({ ...GALLERY_URL_DEFAULTS, camera: 'X-T5', takenFrom: 100, takenTo: 200 }),
    ).toEqual({ view: 'all', cameraModel: 'X-T5', takenFrom: 100, takenTo: 200 })
  })

  it('排序只有 importedAt / takenAt 两种服务端口径（验收脚本读 ?sort=takenAt）', () => {
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, sort: 'takenAt' }).sort).toBe('takenAt')
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, sort: 'name' }).sort).toBeUndefined()
  })

  it('tab=recent → view=recent（最近导入走服务端另一条查询）', () => {
    expect(galleryFilterOf({ ...GALLERY_URL_DEFAULTS, tab: 'recent' }).view).toBe('recent')
  })
})

describe('galleryUrlPatchOf：LibraryFilter → URL patch（往返一致）', () => {
  const cases: LibraryFilter[] = [
    { view: 'all' },
    { view: 'recent' },
    { view: 'all', dir: '/test-fixture/corpus/batch4' },
    { view: 'all', tag: '雪山' },
    { view: 'all', untagged: true },
    { view: 'all', missing: 1, broken: 1 },
    { view: 'all', aspect: 'square', cameraModel: 'ILCE-7M3', takenFrom: 1700000000, takenTo: 1710000000 },
    { view: 'all', sort: 'takenAt' },
    { view: 'recent', dir: '/test-fixture/a', tag: '风景', untagged: true, sort: 'importedAt' },
  ]

  it('每个筛选都能往返（URL 写出去 → 读回来 = 同一个 filter）', () => {
    for (const f of cases) {
      const patch = galleryUrlPatchOf(f)
      const back = galleryFilterOf({ ...GALLERY_URL_DEFAULTS, ...patch })
      // view 之外的 undefined 字段在往返后可能显式为 undefined —— 用 JSON 口径比较
      expect(JSON.parse(JSON.stringify(back))).toEqual(JSON.parse(JSON.stringify(f)))
    }
  })

  it('patch 只含筛选键，不含浮层/选择集（开浮层不该改写筛选，反之亦然）', () => {
    const patch = galleryUrlPatchOf({ view: 'all', dir: '/test-fixture/a' })
    expect(Object.keys(patch).sort()).toEqual(
      [
        'aspect',
        'broken',
        'camera',
        'dir',
        'missing',
        // KI-16 #7：从侧栏/目录树换一套条件 = 不再属于原来那个智能文件夹（解除 sf 绑定；
        // 芯片逐个移除走 galleryChipPatchOf，不在此列 —— 那种情况仍留在文件夹上下文中）
        'sf',
        'sort',
        'tab',
        'takenFrom',
        'takenTo',
        'tags',
        'untagged',
      ].sort(),
    )
  })

  it('去掉维度后用默认值表示 → 该键会被 urlState 从 URL 移除（不写空串）', () => {
    const patch = galleryUrlPatchOf({ view: 'all' })
    expect(patch.dir).toBe('')
    expect(patch.tags).toBe('')
    expect(patch.untagged).toBe(false)
    expect(patch.open).toBeUndefined()
    expect(patch.ids).toBeUndefined()
  })
})

describe('galleryChipPatchOf：芯片一键移除只动一个维度', () => {
  it('目录 / 标签各自回到默认值（KI-11：评分芯片已移除）', () => {
    expect(galleryChipPatchOf('dir')).toEqual({ dir: '' })
    expect(galleryChipPatchOf('tag')).toEqual({ tags: '' })
  })

  it('布尔与枚举维度', () => {
    expect(galleryChipPatchOf('untagged')).toEqual({ untagged: false })
    expect(galleryChipPatchOf('missing')).toEqual({ missing: false })
    expect(galleryChipPatchOf('broken')).toEqual({ broken: false })
    expect(galleryChipPatchOf('aspect')).toEqual({ aspect: '' })
    expect(galleryChipPatchOf('cameraModel')).toEqual({ camera: '' })
  })

  it('拍摄区间是一个芯片 → 两端一起清', () => {
    expect(galleryChipPatchOf('takenRange')).toEqual({ takenFrom: 0, takenTo: 0 })
  })

  it('query 不在这里处理（关键词走 q，由页面单独清）', () => {
    expect(galleryChipPatchOf('query')).toEqual({})
  })
})

describe('galleryClearPatch：来源提示海报的「清除」', () => {
  it('清掉全部筛选键，但不动浮层与选择集', () => {
    const patch = galleryClearPatch()
    expect(patch).toMatchObject({
      q: '',
      dir: '',
      tags: '',
      rating: 0,
      untagged: false,
      missing: false,
      broken: false,
      aspect: '',
      camera: '',
      takenFrom: 0,
      takenTo: 0,
      sort: '',
    })
    expect(patch.open).toBeUndefined()
    expect(patch.ids).toBeUndefined()
    expect(patch.tab).toBeUndefined()
  })
})

/**
 * 回归（卡 t_58715952）：选择集写回 `?ids=` 不应改变筛选签名。
 * 图库首页的加载副作用原先以 `filter` 对象为依赖：选择集一变 → ui.value 变新对象 →
 * filter 重新 memo → 重取列表，把「删除→撤销」窗口期内还没落库的卡片读回来、盖掉乐观摘除。
 * 现在依赖 `filterKey(filter)`（JSON 内容签名），这条断言守住「ids 不参与筛选」的前提。
 */
describe('选择集不参与筛选签名（删除→撤销窗口期不被误重取）', () => {
  it('ids 变化不改变 LibraryFilter 内容，也不改变 filterKey 签名', async () => {
    const { filterKey } = await import('../src/components/LibraryTree')
    const base = { ...GALLERY_URL_DEFAULTS, dir: '/test-fixture/a', tags: '户外', sort: 'takenAt' as const }
    const withIds = { ...base, ids: [11, 12] }
    const f0 = galleryFilterOf(base)
    const f1 = galleryFilterOf(withIds)
    expect(f1).toEqual(f0)
    expect(filterKey(f1)).toBe(filterKey(f0))
  })
})


/**
 * 卡 t_4bc7fa24：导出「当前筛选结果」范围的条件映射。
 * 图库列表查询与导出 filter **必须是同一个映射**（queryOf 已改为委托本函数）——
 * 两份映射漂移的后果就是 DT4 那类「预览正确、导出跑题」，而且界面不会报错。
 */
describe('exportFilterOf：筛选条件 → 导出 filter（图库 queryOf 同一实现）', () => {
  it('多维度全量映射（目录/标签/未打标/失联/无法预览/画幅/相机/时间/排序；KI-11 起无评分维度）', () => {
    const f: LibraryFilter = {
      view: 'all',
      dir: '/test-fixture/in',
      tag: '户外',
      untagged: true,
      missing: 1,
      broken: 1,
      aspect: 'landscape',
      cameraModel: 'X-T5',
      takenFrom: 1000,
      takenTo: 2000,
      sort: 'takenAt',
    }
    expect(exportFilterOf(f)).toEqual({
      dirPrefix: '/test-fixture/in',
      tag: '户外',
      untagged: true,
      missing: 1,
      broken: 1,
      aspect: 'landscape',
      cameraModel: 'X-T5',
      takenFrom: 1000,
      takenTo: 2000,
      sort: 'takenAt',
    })
  })

  it('空条件 → 显式 {}（整库；不给 undefined —— 后端 build_spec 会判「来源非法」）', () => {
    expect(exportFilterOf({ view: 'all' })).toEqual({})
  })

  /**
   * C7（卡 t_eadc13a3）：关键词是**页面态**（不进 LibraryFilter / 不持久化），但导出
   * 「当前筛选结果」时**必须**一起带上 —— 否则图库按「雪山」筛出 2 张、导出却是整库，
   * 这是最坏的一类数据事故（用户拿到一堆没想导的图，还不会报错）。
   */
  it('关键词 q 一起映射（trim；空串不落键 = 整库）', () => {
    expect(exportFilterOf({ view: 'all' }, '雪山')).toEqual({ q: '雪山' })
    expect(exportFilterOf({ view: 'all', tag: '户外' }, '  雪山 ')).toEqual({
      tag: '户外',
      q: '雪山',
    })
    expect(exportFilterOf({ view: 'all' }, '   ')).toEqual({})
    expect(exportFilterOf({ view: 'all' }, '')).toEqual({})
  })

  it('最近视图 → null（视图不是筛选条件；后端 ImportFilter 无 recent 字段，硬塞会静默变整库）', () => {
    expect(exportFilterOf({ view: 'recent' })).toBeNull()
  })

  it('不得带分页键（limit/offset 是取数参数，不是筛选语义）', () => {
    const q = exportFilterOf({ view: 'all', tag: 'a' })!
    expect(q).not.toHaveProperty('limit')
    expect(q).not.toHaveProperty('offset')
  })
})

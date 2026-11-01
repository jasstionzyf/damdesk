/**
 * P1-2 / P1-4 可用性回归（卡 t_bf8f7e95；删除确认已合流到 lib/confirm.tsx）—— 用 react-dom/server 渲染成 HTML，
 * 断言里带的就是可核对 DOM 文本（node 环境，无 jsdom）。
 *
 * 覆盖三条「用户会以为图丢了 / 会误删」的坑：
 *  1. 筛选生效时列表顶部必须有醒目横幅（且 0 命中要说明「不是空库」）；
 *  
 *  3. 合规筛选不跨冷启动复活（localStorage 落盘剔除 compliance）。
 */
import { describe, expect, it, vi } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { FilterBanner } from '../src/components/FilterBanner'
import { ShortcutsHelp } from '../src/components/ShortcutsHelp'
import { chipsOf, type LibraryFilter } from '../src/components/LibraryTree'

// i18n-C（卡 t_06629257）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
// jsdom 的 navigator.language 默认 en-US（会渲染英文）。这里把语言钉成 zh-Hans ——
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变、强度不降，只是显式声明
// 「本文件验中文文案」。vi.hoisted 早于所有 import 执行，故早于 i18n 模块初始化。
vi.hoisted(() => {
  // jsdom 文件有真实 localStorage；node 环境的纯 SSR 渲染测试没有它 ——
  // 给一个最小桩，目的只有一个：让 i18n 初始化时 resolveInitialLocale() 读到 zh-Hans。
  try {
    if (typeof localStorage !== 'undefined' && localStorage) {
      localStorage.setItem('dam.locale', 'zh-Hans')
      return
    }
  } catch {
    /* 落桩 */
  }
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => (k === 'dam.locale' ? 'zh-Hans' : null),
    setItem: () => {},
    removeItem: () => {},
    clear: () => {},
    key: () => null,
    length: 0,
  })
})

const noop = () => {}

describe('P1-2 筛选横幅（FilterBanner）', () => {
  it('无筛选 → 不渲染（不占位、不吓人）', () => {
    const html = renderToStaticMarkup(
      <FilterBanner chips={[]} total={0} onRemoveChip={noop} onClear={noop} />,
    )
    expect(html).toBe('')
  })

  it('筛选命中 0 张 → 横幅说明「图库不是空的」+ 清除按钮', () => {
    const filter: LibraryFilter = { view: 'all', tag: '雪山' }
    const chips = chipsOf(filter)
    const html = renderToStaticMarkup(
      <FilterBanner chips={chips} total={0} onRemoveChip={noop} onClear={noop} />,
    )
    console.log('[DOM] filter-banner(0) =', html)
    expect(html).toContain('data-testid="filter-banner"')
    expect(html).toContain('data-empty="1"')
    expect(html).toContain('筛选中：标签 雪山')
    expect(html).toContain('命中 0 张 —— 图库不是空的，是当前条件没有匹配')
    // 一键清除 + 逐条移除都在
    expect(html).toContain('data-testid="filter-clear"')
    expect(html).toContain('data-testid="chip-remove-tag"')
    expect(html).toContain('清除筛选')
  })

  it('有命中 → 报出服务端 total（不是已加载条数）', () => {
    const chips = chipsOf({ view: 'all', dir: '/test-fixture/a/b', tag: '雪山' })
    const html = renderToStaticMarkup(
      <FilterBanner chips={chips} total={37} onRemoveChip={noop} onClear={noop} />,
    )
    expect(html).toContain('data-empty="0"')
    expect(html).toContain('筛选中：目录 b · 标签 雪山')
    expect(html).toContain('命中 37 张')
  })
})

describe('P1-4 键位可达性（ShortcutsHelp）', () => {
  it('速查面板列出 Delete 删除键位（可发现性：不写在面板里的键等于不存在）', () => {
    const html = renderToStaticMarkup(<ShortcutsHelp open onClose={noop} />)
    expect(html).toContain('Delete / Backspace')
    expect(html).toContain('删除选择集（移入回收站，二次确认后可撤销）')
  })
})

describe('P1-2 图库筛选持久化（persistedGallery）', () => {
  it('DECouple-FE：判定维度已不是图库筛选，旧档里的 compliance 读时丢弃', async () => {
    const store = new Map<string, string>()
    vi.stubGlobal('localStorage', {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
      removeItem: (k: string) => void store.delete(k),
    })
    const { loadPersistedFilter, savePersistedFilter } = await import('../src/lib/persistedGallery')

    // 图库只认资产域条件：落盘的永远是纯粹的资产筛选（判定维度已整体下线）
    savePersistedFilter({ view: 'all', tag: '狗' })
    const raw = store.get('dam.gallery.filter') ?? ''
    expect(JSON.parse(raw)).toEqual({ view: 'all', tag: '狗' })

    // 老版本已写进 localStorage 的脏数据 → 读的时候丢掉，不会让图库恒 0 张
    store.set('dam.gallery.filter', JSON.stringify({ view: 'all', compliance: 'fail', tag: '狗' }))
    const back = loadPersistedFilter()
    expect(back).toEqual({ view: 'all', tag: '狗' })

    // 其余维度仍然跨重启保留（目录/未打标）；KI-11：旧档残留的评分下限随读取一起丢弃
    savePersistedFilter({ view: 'all', dir: '/test-fixture/a', untagged: true } as never)
    expect(loadPersistedFilter()).toEqual({ view: 'all', dir: '/test-fixture/a', untagged: true })
    store.set('dam.gallery.filter', JSON.stringify({ view: 'all', dir: '/test-fixture/a', minRating: 3 }))
    expect(loadPersistedFilter()).toEqual({ view: 'all', dir: '/test-fixture/a' })
    vi.unstubAllGlobals()
  })
})

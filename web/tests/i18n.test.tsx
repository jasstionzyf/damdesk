// @vitest-environment jsdom
/**
 * i18n 基座单测（i18n-A，卡 t_2a977c80）。
 *
 * 覆盖 plan A 卡第 5 条验收：默认回退、缺 key 回退原文、插值、ctx 消歧、
 * 切换写 localStorage + document.lang、navigator.language 判定 4 例、tGlobal 全局值。
 */
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { cleanup, fireEvent, render, screen } from '@testing-library/react'

import {
  LOCALE_STORAGE_KEY,
  LocaleProvider,
  getGlobalLocale,
  subscribeLocaleChange,
  tGlobal,
  useI18n,
} from '../src/i18n'

/** 探针组件：把 context 关键值渲染出来供断言 */
function Probe() {
  const { locale, setLocale, t } = useI18n()
  return (
    <div>
      <span data-testid="locale">{locale}</span>
      <span data-testid="interp">{t('正在导入 {done}/{total}', { done: 1, total: 5 })}</span>
      <span data-testid="missing">{t('这句中文没有任何翻译条目')}</span>
      <span data-testid="ctx">{t('打开', { ctx: 'menu' })}</span>
      {/* 「缺 key 回退」夹具必须是**没有词条**的中文（'打开' 已是真实界面词条 → 换成它） */}
      <span data-testid="plain">{t('打开抽屉')}</span>
      <span data-testid="hit">{t('打开')}</span>
      <span data-testid="global">{tGlobal('切换立即生效，无需重启。')}</span>
      <button data-testid="switch" onClick={() => setLocale('en-US')}>
        switch
      </button>
    </div>
  )
}

function renderProbe() {
  return render(
    <LocaleProvider>
      <Probe />
    </LocaleProvider>,
  )
}

const origLang = Object.getOwnPropertyDescriptor(window.navigator, 'language')

// vitest 未开 globals → RTL 自动 cleanup 不生效，手动挂（否则探针残留 body，getByTestId 撞重复元素）
afterEach(() => {
  cleanup()
})

afterEach(() => {
  if (origLang) Object.defineProperty(window.navigator, 'language', origLang)
  window.localStorage.clear()
})

describe('i18n 基座（t_2a977c80）', () => {
  beforeEach(() => {
    window.localStorage.clear()
    // jsdom navigator.language 默认 'en-US'：默认中文环境，4 例判定用例各自覆盖
    Object.defineProperty(window.navigator, 'language', { value: 'zh-CN', configurable: true })
  })

  it('缺 key 回退原文：en 环境下无词条的中文 key 原样返回（不白屏不显示 key 标识）', () => {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, 'en-US')
    renderProbe()
    expect(screen.getByTestId('missing').textContent).toBe('这句中文没有任何翻译条目')
    expect(screen.getByTestId('plain').textContent).toBe('打开抽屉')
  })

  it('插值：中文原文兜底替换 {done}/{total}；en 命中词典模板', () => {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, 'en-US')
    renderProbe()
    expect(screen.getByTestId('interp').textContent).toBe('Importing 1/5')
  })

  it('ctx 消歧：`打开 [menu]` 命中 en 专译，无 ctx 走缺 key 回退', () => {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, 'en-US')
    renderProbe()
    expect(screen.getByTestId('ctx').textContent).toBe('Open')
    // 真实词条（ResultTable 的「打开」链接）在 en 下命中词典；无 ctx 的 '打开抽屉' 无词条 → 回退原文
    expect(screen.getByTestId('hit').textContent).toBe('Open')
    expect(screen.getByTestId('plain').textContent).toBe('打开抽屉')
  })

  it('切换：写 localStorage + documentElement.lang 双写 + 组件状态同步', () => {
    renderProbe()
    expect(screen.getByTestId('locale').textContent).toBe('zh-Hans')
    fireEvent.click(screen.getByTestId('switch'))
    expect(screen.getByTestId('locale').textContent).toBe('en-US')
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe('en-US')
    expect(document.documentElement.lang).toBe('en-US')
    expect(document.documentElement.dir).toBe('ltr')
  })

  it('tGlobal 在切换后取新值；订阅者收到通知', () => {
    let notified = 0
    const unsub = subscribeLocaleChange(() => {
      notified += 1
    })
    renderProbe()
    // LocaleProvider mount 已把模块级 globalLocale 同步为初始判定值
    expect(getGlobalLocale()).toBe('zh-Hans')
    expect(screen.getByTestId('global').textContent).toBe('切换立即生效，无需重启。')
    fireEvent.click(screen.getByTestId('switch'))
    expect(screen.getByTestId('global').textContent).toBe('Applies immediately, no restart needed')
    expect(getGlobalLocale()).toBe('en-US')
    expect(notified).toBeGreaterThanOrEqual(1)
    unsub()
  })

  it('tGlobal 参数化：非组件模块插值可用', () => {
    renderProbe()
    expect(tGlobal('正在导入 {done}/{total}', { done: 2, total: 9 })).toBe('正在导入 2/9')
  })

  it('navigator.language 判定 4 例（localStorage 无值时）', () => {
    const cases: Array<[string, string]> = [
      ['zh-CN', 'zh-Hans'],
      ['zh-TW', 'zh-Hans'],
      ['en-US', 'en-US'],
      ['ja-JP', 'en-US'],
    ]
    for (const [lang, expected] of cases) {
      Object.defineProperty(window.navigator, 'language', { value: lang, configurable: true })
      const { unmount } = renderProbe()
      expect(screen.getByTestId('locale').textContent, `navigator.language=${lang}`).toBe(expected)
      unmount()
    }
  })

  it('localStorage 存了非法值 → 落 navigator 判定，不崩', () => {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, 'klingon')
    Object.defineProperty(window.navigator, 'language', { value: 'zh-CN', configurable: true })
    renderProbe()
    expect(screen.getByTestId('locale').textContent).toBe('zh-Hans')
  })
})

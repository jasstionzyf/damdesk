/**
 * i18n-F（卡 t_d4750e8d）：行业包自带文案的取值（pickI18n / pickI18nOr / usePackTextLocale）。
 *
 * 回退链与 Rust `core_config::packs::LocalizedText::pick` 一致：
 * 当前 locale → zh-Hans → 任意一条 → 空串；字符串形态 = 全语言通用。
 */
// @vitest-environment jsdom
import { act, renderHook } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'

import { useI18n } from '../src/i18n'
import {
  pickI18n,
  pickI18nOr,
  usePackTextLocale,
} from '../src/i18n/pickI18n'

const zhName = { 'zh-Hans': '合规检查', 'en-US': 'Compliance Check' }

afterEach(() => {
  try {
    localStorage.removeItem('dam.locale')
  } catch {
    /* 忽略 */
  }
})

describe('pickI18n：两种形态与回退链', () => {
  it('字符串形态 = 全语言通用（老包行为不变）', () => {
    expect(pickI18n('合规检查', 'zh-Hans')).toBe('合规检查')
    expect(pickI18n('合规检查', 'en-US')).toBe('合规检查')
  })

  it('对象形态：命中当前语言', () => {
    expect(pickI18n(zhName, 'en-US')).toBe('Compliance Check')
    expect(pickI18n(zhName, 'zh-Hans')).toBe('合规检查')
  })

  it('缺当前语言 → 回退 zh-Hans（与 Rust pick 同链）', () => {
    expect(pickI18n({ 'zh-Hans': '白底图纯白' }, 'en-US')).toBe('白底图纯白')
  })

  it('缺 zh-Hans → 回退任意一条（不崩）', () => {
    expect(pickI18n({ 'en-US': 'Only English' }, 'zh-Hans')).toBe('Only English')
  })

  it('缺失/坏形态 → 空串（不显示 key、不抛错）', () => {
    expect(pickI18n(null, 'en-US')).toBe('')
    expect(pickI18n(undefined, 'en-US')).toBe('')
    expect(pickI18n({}, 'en-US')).toBe('')
    expect(pickI18n(['坏形态'], 'en-US')).toBe('')
    expect(pickI18n({ 'zh-Hans': 3 as unknown as string }, 'en-US')).toBe('')
  })

  it('pickI18nOr：i18n 命中优先，否则回退宿主已取好的展示串', () => {
    expect(pickI18nOr(zhName, '电商主图合规（旧）', 'en-US')).toBe('Compliance Check')
    expect(pickI18nOr(null, '电商主图合规（旧）', 'en-US')).toBe('电商主图合规（旧）')
    expect(pickI18nOr({}, undefined, 'zh-Hans')).toBe('')
  })
})

describe('usePackTextLocale：locale 源与订阅', () => {
  it('跟随 i18n 全局 locale 并即时订阅（切换即生效，持久面仍是 dam.locale）', () => {
    // i18n-G 合流：全局 locale 在模块初始化时定一次（主干 setup 钉 zh-Hans），
    // 直接写 localStorage 不会改已初始化的 store ⇒ 走与设置页同一条切换 API（useI18n().setLocale）。
    const { result: i18n } = renderHook(() => useI18n())
    act(() => i18n.current.setLocale('en-US'))
    const { result } = renderHook(() => usePackTextLocale())
    expect(result.current).toBe('en-US')
    expect(localStorage.getItem('dam.locale')).toBe('en-US')
    act(() => i18n.current.setLocale('zh-Hans'))
  })

  it('无存储值时按 navigator.language 判定（zh* → zh-Hans）', () => {
    const { result } = renderHook(() => usePackTextLocale())
    expect(['zh-Hans', 'en-US']).toContain(result.current)
  })
})

/**
 * 行业包自带文案的取值（i18n-F，卡 t_d4750e8d）。
 *
 * 包提供的展示文案（manifest `name`/`short_name`/`summary`、rules `rules[].name`、
 * `result_columns[].label`）有两种形态（协议 §4.0「可选多语言字段」，不升 api_version）：
 * - 字符串 = 全语言通用（老包行为，原样返回）；
 * - 对象 `{"zh-Hans": "…", "en-US": "…"}` = 按语言给值。
 *
 * 取值回退链与 Rust 侧 `core_config::packs::LocalizedText::pick` 完全一致：
 * **当前 locale → zh-Hans → 任意一条 → 空串**（永不抛错、永不显示 key）。
 * 后端已按请求 `Accept-Language` 预取过一次（`name` 恒有值），对象字段（`nameI18n` /
 * `ruleNameI18n` / `labelI18n`）是原样透传 —— 前端用它取值，切语言即时重渲染、不重拉接口。
 *
 * locale 源 = i18n-A 的全局 locale（localStorage `dam.locale`）：
 * 组件里用 [`usePackTextLocale`] 订阅变更（无 LocaleProvider 也能用，宿主壳/测试不炸）。
 */
import { useEffect, useState } from 'react'
import { getGlobalLocale, subscribeLocaleChange, type LocaleCode } from './index'

/** 包提供的可本地化文案（协议 §4.0）：字符串 | {"locale": 文案} | 缺失。 */
export type PackText = string | Record<string, string> | null | undefined

/** 缺当前语言时的回退键（与 Rust `I18N_FALLBACK_LOCALE` 同值）。 */
export const I18N_FALLBACK_LOCALE = 'zh-Hans'

/**
 * 按回退链取包文案：`locale` → `zh-Hans` → 任意一条 → 空串。
 * 字符串形态直接原样返回（全语言通用）；null/坏形态（数组、值非字符串）返回空串。
 */
export function pickI18n(value: PackText, locale: LocaleCode | string): string {
  if (value == null) return ''
  if (typeof value === 'string') return value
  if (typeof value !== 'object' || Array.isArray(value)) return ''
  const entries = Object.entries(value).filter(([, v]) => typeof v === 'string')
  if (!entries.length) return ''
  const hit =
    (locale ? value[locale] : undefined) ??
    value[I18N_FALLBACK_LOCALE] ??
    entries[0][1]
  return typeof hit === 'string' ? hit : ''
}

/** 显示值：i18n 对象命中优先；否则回退宿主按 Accept-Language 已取好的 `fallback`。 */
export function pickI18nOr(
  value: PackText,
  fallback: string | null | undefined,
  locale: LocaleCode | string,
): string {
  return pickI18n(value, locale) || fallback || ''
}

/**
 * 组件里取当前 locale（订阅 i18n-A 的全局变更；语言切换 → 重渲染包文案）。
 * 为什么不用 `useI18n()`：它会因 LocaleProvider 缺失而抛错（接错层要立即暴露是 A 的
 * 设计）；包文案渲染面（PackManager / 合规面板）必须无 Provider 也安全。
 */
export function usePackTextLocale(): LocaleCode {
  const [locale, setLocale] = useState<LocaleCode>(() => getGlobalLocale())
  useEffect(
    () => subscribeLocaleChange(() => setLocale(getGlobalLocale())),
    [],
  )
  return locale
}

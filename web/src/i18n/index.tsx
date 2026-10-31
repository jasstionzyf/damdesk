/**
 * i18n 基座（i18n-A，卡 task）。
 *
 * 设计（plan §3/§A 卡）：
 *  - **source-text-as-key**：key 就是界面中文原文。中文薄壳字典查不中 → 原样返回 key，
 *    因此中文永远是安全的兜底语言；en 字典缺词条也不会白屏（显示中文）。
 *  - **自写 t()，不引 i18next**：零依赖、无初始化异步、bundle 不膨胀；缺口只有一个
 *    （无复数规则），界面文案当前用不到，B/C/D 批遇到再议。
 *  - 组件树内用 `useI18n()`；非组件 lib 层用 `tGlobal()`（读模块级当前 locale，
 *    每次调用取最新值）+ `subscribeLocaleChange()`（缓存了译文的 lib 模块订阅后重算）。
 *  - 持久化 localStorage `dam.locale`（读写都容错：隐身模式/禁存储不崩）；
 *    首启无存储值 → `navigator.language` 前缀 `zh*` 判中文，其余一律 en-US。
 *  - `document.documentElement.lang` 双写（切换即同步），`dir` 恒 `ltr`
 *    （B/C/D 全量走查时再确认无 RTL 需求）。
 */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from 'react'
import zhHans from './dict.zh-Hans'
import enUS from './dict.en-US'

export type LocaleCode = 'zh-Hans' | 'en-US'

export interface LocaleEntry {
  code: LocaleCode
  /** 语言分区下拉里的展示名（始终本地语言书写，不翻译） */
  label: string
}

export const LOCALES: LocaleEntry[] = [
  { code: 'zh-Hans', label: '简体中文' },
  { code: 'en-US', label: 'English' },
]

export const LOCALE_STORAGE_KEY = 'dam.locale'

const LOCALE_CODES = new Set<string>(LOCALES.map((l) => l.code))

/** en 词典；zh-Hans 走薄壳兜底（key 即中文原文），字典仅作查表一致性占位 */
const DICTS: Record<LocaleCode, Record<string, string>> = {
  'zh-Hans': zhHans,
  'en-US': enUS,
}

/** t() 的参数：占位符值 + 可选 ctx 消歧 */
export type TParams = Record<string, string | number> & { ctx?: string }
export type Translate = (key: string, params?: TParams) => string

// ---------------------------------------------------------------------------
// locale 解析与持久化
// ---------------------------------------------------------------------------

function normalizeLocale(raw: string | null | undefined): LocaleCode | null {
  return raw != null && LOCALE_CODES.has(raw) ? (raw as LocaleCode) : null
}

function localeFromNavigator(): LocaleCode {
  try {
    const lang = (navigator.language || '').trim().toLowerCase()
    if (lang.startsWith('zh')) return 'zh-Hans'
  } catch {
    /* 非 DOM 环境（测试/SSR）：走默认 */
  }
  return 'en-US'
}

/** 启动 locale：localStorage 有效值优先，否则 navigator.language 判定 */
export function resolveInitialLocale(): LocaleCode {
  try {
    const stored = normalizeLocale(localStorage.getItem(LOCALE_STORAGE_KEY))
    if (stored) return stored
  } catch {
    /* localStorage 不可用（禁存储/沙箱）：落到 navigator 判定 */
  }
  return localeFromNavigator()
}

function applyDocumentLang(locale: LocaleCode): void {
  try {
    document.documentElement.lang = locale
    document.documentElement.dir = 'ltr'
  } catch {
    /* 无 document（node 测试环境）：跳过 */
  }
}

// ---------------------------------------------------------------------------
// tGlobal：非组件模块（lib 层）使用的全局翻译
// ---------------------------------------------------------------------------

let globalLocale: LocaleCode = resolveInitialLocale()
const globalListeners = new Set<() => void>()

/** 模块级当前 locale（tGlobal 每次调用读它，无过期缓存） */
export function getGlobalLocale(): LocaleCode {
  return globalLocale
}

/**
 * 订阅 locale 变更（lib 层缓存了译文的模块用它触发重算）。
 * 返回退订函数。listener 抛错不影响其它 listener。
 */
export function subscribeLocaleChange(listener: () => void): () => void {
  globalListeners.add(listener)
  return () => {
    globalListeners.delete(listener)
  }
}

function setGlobalLocale(next: LocaleCode): void {
  globalLocale = next
  try {
    localStorage.setItem(LOCALE_STORAGE_KEY, next)
  } catch {
    /* 存储不可用：本次会话内仍生效，下次打开回落判定 */
  }
  applyDocumentLang(next)
  for (const listener of Array.from(globalListeners)) {
    try {
      listener()
    } catch {
      /* 单个订阅者异常不阻断切换 */
    }
  }
}

/** 非组件模块用：按当前全局 locale 翻译（每次调用取最新值） */
export function tGlobal(key: string, params?: TParams): string {
  return translate(globalLocale, key, params)
}

// ---------------------------------------------------------------------------
// 翻译核心
// ---------------------------------------------------------------------------

function format(template: string, params?: TParams): string {
  if (!params) return template
  return template.replace(/\{(\w+)\}/g, (match, name: string) => {
    if (name === 'ctx' || !(name in params)) return match
    return String(params[name])
  })
}

/** 翻译核心：ctx 消歧（`key [ctx]`）→ 普通 key → 缺条目回退原文（中文 key） */
export function translate(locale: LocaleCode, key: string, params?: TParams): string {
  let tpl: string | undefined
  const ctx = params?.ctx
  if (ctx) tpl = DICTS[locale][`${key} [${ctx}]`]
  if (tpl === undefined) tpl = DICTS[locale][key]
  return format(tpl ?? key, params)
}

// ---------------------------------------------------------------------------
// React 绑定
// ---------------------------------------------------------------------------

interface I18nContextValue {
  locale: LocaleCode
  setLocale: (locale: LocaleCode) => void
  t: Translate
}

const I18nContext = createContext<I18nContextValue | null>(null)

/**
 * locale Provider —— 包在全部 Provider 外层（toast/confirm 也要取到 locale）。
 * 初始化**每次 mount 重读** resolveInitialLocale()（而非模块级缓存）：
 * localStorage 是唯一持久面，读它永远安全且可测。
 */
export function LocaleProvider({ children }: { children: ReactNode }) {
  const [locale, setLocaleState] = useState<LocaleCode>(() => {
    const initial = resolveInitialLocale()
    globalLocale = initial
    return initial
  })

  const setLocale = useCallback((next: LocaleCode) => {
    if (!LOCALE_CODES.has(next)) return
    setGlobalLocale(next)
    setLocaleState(next)
  }, [])

  useEffect(() => {
    applyDocumentLang(locale)
  }, [locale])

  const t = useCallback<Translate>((key, params) => translate(locale, key, params), [locale])

  const value = useMemo(() => ({ locale, setLocale, t }), [locale, setLocale, t])
  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>
}

/**
 * 组件树内取 { locale, setLocale, t }。
 *
 * LocaleProvider 外调用**不抛错**：降级为「跟随全局 locale」（useSyncExternalStore
 * 订阅模块级状态，setLocale 同样生效、组件同样重渲）。原因：大量既有/后续测试直接
 * render 页面组件（不经过 main.tsx 的 Provider 栈），基座强制 Provider 会把迁移成本
 * 扩散到每一个测试文件；生产树里 main.tsx 恒有 Provider，两条路径行为一致
 * （同一 resolveInitialLocale 判定、同一持久化）。
 */
export function useI18n(): I18nContextValue {
  const ctx = useContext(I18nContext)
  // useSyncExternalStore 无条件调用（hooks 顺序稳定）；有 ctx 时其结果仅被忽略
  const subscribe = useCallback((listener: () => void) => subscribeLocaleChange(listener), [])
  const globalLocale = useSyncExternalStore(subscribe, getGlobalLocale, getGlobalLocale)
  const fallback = useMemo<I18nContextValue>(() => {
    const setLocale = (next: LocaleCode) => {
      if (LOCALE_CODES.has(next)) setGlobalLocale(next)
    }
    return { locale: globalLocale, setLocale, t: (key: string, params?: TParams) => translate(globalLocale, key, params) }
  }, [globalLocale])
  return ctx ?? fallback
}

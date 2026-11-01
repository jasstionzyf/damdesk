/**
 * URL 单一真源（URL-STATE-1 / 卡 t_4b86ba6c）
 *
 * 规则（钉死，页面里不要再各自发明）：
 *  1. 筛选 / 排序 / 关键词 / 当前 tab / 打开中的浮层 / 选择集 → 全部进 URL query；URL 是唯一真源。
 *  2. localStorage / sessionStorage 只存「用户默认偏好」：URL 缺该键时才用它补一次，
 *     补完立刻写回 URL（replace），并给页面一个 `prefApplied` 标记 → 页面必须显示可关闭的
 *     「来源提示」横幅（含命中数与「清除」），不得静默生效。
 *  3. 纯筛选变更 / 浮层关闭用 replace（不制造历史垃圾）；浮层打开、跨区跳转用 push
 *     （浏览器 back 优先关浮层）。
 *  4. key 顺序稳定（KEY_ORDER + 未知 key 字母序）——同一份状态永远序列化成同一串，
 *     便于比较/去重/断言。
 *  5. 空值语义：`''` / `null` / `undefined` / `[]` / `NaN` 一律视为「该键不存在」，不写进 URL。
 *
 * 值编码（无 schema，按默认值类型解码，非法值直接回退默认值）：
 *  boolean → '1'/'0'；number → 十进制；string → 原样；number[] → 逗号分隔；其它 → JSON。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useSearchParams } from 'react-router-dom'

/** 稳定 key 顺序（新增参数请加在这里，不要在页面里拼字符串） */
export const KEY_ORDER = [
  'q',
  'mode',
  'asset',
  'tab',
  'open',
  'sort',
  'dir',
  'path',
  'tags',
  // t_cafb2e45：`/tags?key=<分组>` 深链（标签总览左栏选中项）
  'key',
  'rating',
  'untagged',
  'missing',
  'ext',
  'sf',
  'k',
  'simMin',
  'ids',
] as const

const KNOWN = new Set<string>(KEY_ORDER)

/** 稳定顺序重排（多次 append 的同名 key 保持相对顺序） */
export function orderParams(params: URLSearchParams): URLSearchParams {
  const out = new URLSearchParams()
  for (const k of KEY_ORDER) {
    for (const v of params.getAll(k)) out.append(k, v)
  }
  const rest = [...new Set([...params.keys()])].filter((k) => !KNOWN.has(k)).sort()
  for (const k of rest) {
    for (const v of params.getAll(k)) out.append(k, v)
  }
  return out
}

/** 稳定查询串（含前导 `?`，空则 ''） */
export function queryOf(params: URLSearchParams): string {
  const s = orderParams(params).toString()
  return s ? `?${s}` : ''
}

/** 值 → URL 文本；返回 null = 删除该键 */
export function encodeValue(v: unknown): string | null {
  if (v === null || v === undefined) return null
  if (typeof v === 'boolean') return v ? '1' : '0'
  if (typeof v === 'number') return Number.isFinite(v) ? String(v) : null
  if (typeof v === 'string') return v === '' ? null : v
  if (Array.isArray(v)) {
    const parts = v
      .filter((x) => x !== null && x !== undefined && x !== '')
      .map((x) => String(x))
      .filter((x) => x !== '')
    return parts.length ? parts.join(',') : null
  }
  return JSON.stringify(v)
}

/** URL 文本 → 值：按默认值类型解码；非法/空 → 默认值（脏链接不炸页面） */
export function decodeValue<T>(raw: string, fallback: T): T {
  if (raw === '') return fallback
  if (typeof fallback === 'number') {
    const n = Number(raw)
    return (Number.isFinite(n) ? n : fallback) as unknown as T
  }
  if (typeof fallback === 'boolean') return (raw === '1' || raw === 'true') as unknown as T
  if (Array.isArray(fallback)) {
    const parts = raw
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean)
    if (fallback.every((x) => typeof x === 'number')) {
      const nums = parts.map(Number).filter((n) => Number.isInteger(n) && n > 0)
      return nums as unknown as T
    }
    return parts as unknown as T
  }
  if (fallback !== null && typeof fallback === 'object') {
    try {
      return JSON.parse(raw) as T
    } catch {
      return fallback
    }
  }
  return raw as unknown as T
}

export type Params = Record<string, unknown>

/** 从 URL 读整条记录（缺键 = 默认值） */
export function readRecord<S extends Params>(params: URLSearchParams, defaults: S): S {
  const out: Params = { ...defaults }
  for (const k of Object.keys(defaults)) {
    const raw = params.get(k)
    if (raw === null || raw === '') continue
    out[k] = decodeValue(raw, defaults[k])
  }
  return out as S
}

/** 把一条记录写回 URL：先清掉本记录的键，再只写「非默认」的键；保留本记录之外的参数 */
export function applyRecord<S extends Params>(
  params: URLSearchParams,
  value: S,
  defaults: S,
): URLSearchParams {
  const next = new URLSearchParams(params)
  for (const k of Object.keys(defaults)) next.delete(k)
  for (const k of Object.keys(defaults)) {
    const enc = encodeValue(value[k])
    // 与默认值相同 → 不写（0 / false / '' 都算默认，不进 URL）
    if (enc === null || enc === encodeValue(defaults[k])) continue
    next.set(k, enc)
  }
  return orderParams(next)
}

/** 相对默认值仍然生效的键（用来判断「是否有筛选」→ 决定要不要显示横幅） */
export function activeKeys<S extends Params>(value: S, defaults: S): string[] {
  return Object.keys(defaults).filter((k) => encodeValue(value[k]) !== encodeValue(defaults[k]))
}

export function isDefaultRecord<S extends Params>(value: S, defaults: S): boolean {
  return activeKeys(value, defaults).length === 0
}

/** 跨区跳转用：在 base 路径上打补丁（配合 setPush / `<Link to>`，不用来改当前页筛选） */
export function withParams(base: string, patch: Params): string {
  const [rawPath, rawSearch = ''] = base.split('?')
  const params = new URLSearchParams(rawSearch)
  for (const [k, v] of Object.entries(patch)) {
    const enc = encodeValue(v)
    if (enc === null) params.delete(k)
    else params.set(k, enc)
  }
  return `${rawPath}${queryOf(params)}`
}

/** 本地偏好描述（localStorage / sessionStorage 里的「用户默认值」） */
export interface PrefSpec<S extends Params> {
  /** 横幅文案里的来源名，例如「上次会话」 */
  label: string
  /** 读偏好；null = 没有偏好 */
  load: () => Partial<S> | null
  /** 清除偏好（横幅「清除」/ 用户显式重置时调用） */
  clear: () => void
}

export interface UrlState<S extends Params> {
  /** 当前值（URL 优先，URL 缺键时由偏好补齐一次） */
  value: S
  /** 改值（replace，不制造历史）——纯筛选/排序/关闭浮层用这个 */
  set: (patch: Partial<S>) => void
  /** 改值（push）——浮层打开、跨区跳转用这个，浏览器 back 才能关浮层 */
  setPush: (patch: Partial<S>) => void
  /** 相对默认值仍然生效的键 */
  active: string[]
  /** 初始值里有没有「来自本地偏好」的键（决定是否显示来源提示横幅） */
  prefApplied: boolean
  prefLabel: string
  /** 来源提示是否还该显示（prefApplied 且用户没关掉） */
  showPrefBanner: boolean
  /** 关闭来源提示（只隐藏横幅，不动筛选） */
  dismissPref: () => void
  /** 清除本地偏好（只清存储；页面自己决定要不要把筛选一起重置回默认） */
  clearPreference: () => void
}

/**
 * 单条 URL 记录的读写钩子（一个页面只用一个，避免多个钩子互相覆盖键）。
 * `defaults` 必须是模块级常量（引用稳定）。
 */
export function useUrlState<S extends Params>(defaults: S, pref?: PrefSpec<S>): UrlState<S> {
  const [params, setParams] = useSearchParams()
  const defaultsRef = useRef<S>(defaults)
  defaultsRef.current = defaults

  // 首屏判定用「浏览器真实地址」而不是路由首帧的 params：
  // 数据路由首帧可能还没落定 location，用 window.location 才不会把 URL 里已有的键误判成「偏好在生效」。
  const initialSearchRef = useRef<URLSearchParams | null>(null)
  if (initialSearchRef.current === null) {
    initialSearchRef.current = new URLSearchParams(
      typeof window === 'undefined' ? '' : window.location.search,
    )
  }
  const initialSearch = initialSearchRef.current

  // 偏好只读一次（首挂载）。规则：URL 里只要出现本记录任一键（= 这是一条显式状态链接），
  // 偏好整体不参与（否则分享出去的 `?open=lightbox&ids=…` 会被接收方的个人偏好改味）；
  // 只有「裸链接」才用本地偏好兜底，兜完写回 URL 并显示来源横幅。
  const seedRef = useRef<{ rec: Partial<S>; applied: boolean } | null>(null)
  if (seedRef.current === null) {
    let rec: Partial<S> = {}
    let applied = false
    if (pref) {
      try {
        rec = pref.load() ?? {}
      } catch {
        rec = {}
      }
      const explicitLink = Object.keys(defaultsRef.current).some((k) => initialSearch.has(k))
      if (!explicitLink) {
        for (const k of Object.keys(defaultsRef.current)) {
          const v = (rec as Params)[k]
          if (v === undefined || encodeValue(v) === null) continue
          // 与默认值相同的「偏好」不算生效（persistedGallery 会把 view:'all' 也存下来）
          if (encodeValue(v) === encodeValue(defaultsRef.current[k])) continue
          applied = true
        }
      }
    }
    seedRef.current = { rec, applied }
  }
  const seed = seedRef.current

  // UI-LAYOUT（卡 t_11662290）：清除偏好后必须让 `value` 立刻不再读 seed，
  // 否则「清了偏好 + 删掉 URL 上对应键」会被 seed 就地复活（界面还显示旧值）。
  const [seedEpoch, setSeedEpoch] = useState(0)

  const value = useMemo(() => {
    const fromUrl = readRecord(params, defaultsRef.current)
    if (!seed.applied) return fromUrl
    const out: Params = { ...fromUrl }
    for (const k of Object.keys(defaultsRef.current)) {
      // URL（真实地址）或路由参数里有这个键 → 以 URL 为准，偏好不参与
      if (initialSearch.has(k) || params.has(k)) continue
      const v = (seed.rec as Params)[k]
      if (v === undefined || encodeValue(v) === null) continue
      out[k] = v
    }
    return out as S
  }, [params, seed, seedEpoch])

  // 挂载后把「偏好补齐进来的键」写进 URL → URL 成为唯一真源（replace，不留历史）
  const injected = useRef(false)
  useEffect(() => {
    if (injected.current || !seed.applied) return
    injected.current = true
    const next = applyRecord(params, value, defaultsRef.current)
    if (next.toString() !== orderParams(params).toString()) setParams(next, { replace: true })
    // 只跑一次：此后的变更都由 set()/setPush() 写 URL
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const [banner, setBanner] = useState(seed.applied)
  const [dismissed, setDismissed] = useState(false)

  const write = useCallback(
    (patch: Partial<S>, replace: boolean) => {
      setParams(
        (cur) =>
          applyRecord(
            cur,
            { ...readRecord(cur, defaultsRef.current), ...patch } as S,
            defaultsRef.current,
          ),
        { replace },
      )
    },
    [setParams],
  )

  const set = useCallback((patch: Partial<S>) => write(patch, true), [write])
  const setPush = useCallback((patch: Partial<S>) => write(patch, false), [write])

  const clearPreference = useCallback(() => {
    try {
      pref?.clear()
    } catch {
      /* 存储不可用：静默 */
    }
    // seed 作废：此后 value 只跟 URL 走（否则删掉 URL 键会被 seed 复活，见 seedEpoch 注释）
    seedRef.current = { rec: {}, applied: false }
    setBanner(false)
    setDismissed(true)
    setSeedEpoch((n) => n + 1)
  }, [pref])

  const dismissPref = useCallback(() => {
    setBanner(false)
    setDismissed(true)
  }, [])

  return {
    value,
    set,
    setPush,
    active: activeKeys(value, defaultsRef.current),
    prefApplied: banner,
    prefLabel: pref?.label ?? '',
    showPrefBanner: banner && !dismissed,
    dismissPref,
    clearPreference,
  }
}

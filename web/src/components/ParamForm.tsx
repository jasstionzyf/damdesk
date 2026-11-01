/**
 * `ParamForm` —— 插件参数的**唯一**通用渲染器（设计口径：`plugin-param-schema.md` §2/§3）。
 *
 * 三处复用同一份实现：规格编辑、任务重跑、插件详情页的「参数默认值」只读面板。
 * 加插件、加参数 → **零前端改动**（插件声明 schema，这里按 `type` 映射控件）。
 *
 * 六条硬规则在代码里的落点：
 * 1. 控件映射唯一一套：`type` 是数据语义，插件不能指定控件（这里就是那张映射表）；
 * 2. **默认值在插件里，规格只存 diff**：`value` 只放用户改过的键；改回默认值 → 从 diff 里删掉
 *    （插件升级默认值时旧规格不会被锁死）；
 * 3. 条件显隐只有 `when:{key:[values]}` 一种（不做表达式/脚本）；`when` 不成立的键**从 diff 剔除**
 *    —— 宿主合并时也会剔，但界面先剔一次，用户看到的就是将要提交的东西；
 * 4. 分组 + 高级折叠：基础组直出，`collapsed`/`advanced` 的组默认收起（不许出现参数墙）；
 * 5. 双侧校验：这里只做**即时可用性**（min/max/step/options/required 标记），真校验在宿主与插件；
 * 6. 试跑入口不在这里 —— 它是页头动作，不是表单的一部分。
 *
 * 明确不做（v1）：`pattern` 正则校验、嵌套对象、插件自带 HTML/自定义控件。
 */
import { useMemo } from 'react'
import type { ParamProperty, ParamSchemaDoc } from '../lib/api'
import { useI18n } from '../i18n'

export type ParamFormMode = 'edit' | 'defaults' | 'disabled'

export interface ParamFormProps {
  schema: ParamSchemaDoc
  /** edit 模式：用户改过的键（diff）。defaults/disabled 模式忽略。 */
  value?: Record<string, unknown>
  onChange?: (nextDiff: Record<string, unknown>) => void
  /**
   * edit：可编辑（规格编辑）；
   * defaults：只读回显插件默认值（插件详情页「参数默认值」）；
   * disabled：未安装 → 控件灰态 + 「装好后可用」。
   */
  mode?: ParamFormMode
  testId?: string
}

const EMPTY: Record<string, unknown> = {}

/** 值相等判定（数组/对象按 JSON 比；参数值只有标量和双值数组，不上深比较库） */
function sameValue(a: unknown, b: unknown): boolean {
  if (a === b) return true
  if (a == null || b == null) return false
  if (typeof a === 'object' || typeof b === 'object') {
    try {
      return JSON.stringify(a) === JSON.stringify(b)
    } catch {
      return false
    }
  }
  return false
}

/** 生效值 = diff ⊕ 插件默认值（与宿主合并序的子集：规格层压默认值） */
function effectiveOf(
  props: Record<string, ParamProperty>,
  diff: Record<string, unknown>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {}
  for (const [k, p] of Object.entries(props)) {
    out[k] = k in diff ? diff[k] : p.default
  }
  return out
}

/** `when:{key:[values]}` 成立否（只支持「另一个参数等于某值」这一种） */
function whenOk(p: ParamProperty, eff: Record<string, unknown>): boolean {
  if (!p.when) return true
  return Object.entries(p.when).every(([dep, allowed]) => {
    const v = eff[dep]
    return (allowed as unknown[]).some((a) => sameValue(a, v))
  })
}

/** 剔除 `when` 不成立的键（与宿主 `/judge` 合并时的口径一致） */
export function stripHidden(
  schema: ParamSchemaDoc,
  diff: Record<string, unknown>,
): Record<string, unknown> {
  const props = schema.properties ?? {}
  const eff = effectiveOf(props, diff)
  const out: Record<string, unknown> = {}
  for (const [k, v] of Object.entries(diff)) {
    const p = props[k]
    if (!p) {
      // schema 里不认识的键：不静默丢（宿主会报 params_unknown_key），原样留着让错误可见
      out[k] = v
      continue
    }
    if (whenOk(p, eff)) out[k] = v
  }
  return out
}

interface FieldCtx {
  /** testid 前缀（同一页面可能有两份 ParamForm：插件默认值 + 规格编辑） */
  tid: (suffix: string) => string
  name: string
  p: ParamProperty
  mode: ParamFormMode
  value: unknown
  dirty: boolean
  required: boolean
  set: (v: unknown) => void
  reset: () => void
}

function num(v: unknown): string {
  return typeof v === 'number' && Number.isFinite(v) ? String(v) : ''
}

/** 单个参数行（label + 控件 + 说明 + 「已改/恢复默认」） */
function Field({ ctx }: { ctx: FieldCtx }) {
  const { name, p, mode, value, dirty, required, tid } = ctx
  const { t } = useI18n()
  const ro = mode !== 'edit'
  const base =
    'rounded border border-line bg-panel-2 px-2 py-1 text-xs text-ink outline-none focus:border-accent disabled:cursor-not-allowed disabled:opacity-50'

  const parseNum = (raw: string): number | undefined => {
    if (raw.trim() === '') return undefined
    const n = Number(raw)
    return Number.isFinite(n) ? n : undefined
  }

  let control = null
  if (p.type === 'integer' || p.type === 'number') {
    const hasRange = p.min != null && p.max != null && p.step != null
    const v = typeof value === 'number' ? value : undefined
    control = (
      <span className="flex items-center gap-2">
        {hasRange && (
          <input
            type="range"
            aria-label={t('{title} 滑杆', { title: p.title ?? name })}
            data-testid={tid(`range-${name}`)}
            min={p.min}
            max={p.max}
            step={p.step}
            value={v ?? Number(p.default ?? p.min)}
            disabled={ro}
            onChange={(e) => ctx.set(Number(e.target.value))}
            className="h-1 w-40 accent-accent disabled:opacity-50"
          />
        )}
        <input
          type="number"
          aria-label={p.title ?? name}
          data-testid={tid(`input-${name}`)}
          min={p.min}
          max={p.max}
          step={p.step ?? (p.type === 'integer' ? 1 : 'any')}
          value={num(value)}
          placeholder={num(p.default) || '—'}
          disabled={ro}
          onChange={(e) => ctx.set(parseNum(e.target.value))}
          className={`${base} w-24`}
        />
        {p.unit && <span className="text-[11px] text-ink-dim">{p.unit}</span>}
      </span>
    )
  } else if (p.type === 'enum') {
    const opts = p.options ?? []
    if (opts.length > 0 && opts.length <= 8) {
      control = (
        <span className="flex flex-wrap items-center gap-1" role="group" aria-label={p.title ?? name}>
          {opts.map((o) => {
            const on = sameValue(o, value)
            return (
              <button
                key={String(o)}
                type="button"
                data-testid={tid(`option-${name}-${String(o)}`)}
                aria-pressed={on}
                disabled={ro}
                onClick={() => ctx.set(o)}
                className={`rounded border px-2 py-0.5 text-[11px] disabled:cursor-not-allowed disabled:opacity-50 ${
                  on
                    ? 'border-accent bg-accent/15 text-accent-soft'
                    : 'border-line text-ink-dim hover:border-accent hover:text-accent'
                }`}
              >
                {String(o)}
              </button>
            )
          })}
        </span>
      )
    } else {
      control = (
        <select
          aria-label={p.title ?? name}
          data-testid={tid(`select-${name}`)}
          value={value == null ? '' : String(value)}
          disabled={ro}
          onChange={(e) => {
            const raw = e.target.value
            if (raw === '') return ctx.set(undefined)
            const hit = opts.find((o) => String(o) === raw)
            ctx.set(hit ?? raw)
          }}
          className={base}
        >
          <option value="">{t('（未设置）')}</option>
          {(opts.length > 0 ? opts : [p.default]).filter((o) => o != null).map((o) => (
            <option key={String(o)} value={String(o)}>
              {String(o)}
            </option>
          ))}
        </select>
      )
    }
  } else if (p.type === 'boolean') {
    control = (
      <label className="flex items-center gap-2 text-[11px] text-ink-dim">
        <input
          type="checkbox"
          role="switch"
          aria-label={p.title ?? name}
          data-testid={tid(`toggle-${name}`)}
          checked={value === true}
          disabled={ro}
          onChange={(e) => ctx.set(e.target.checked)}
        />
        {value === true ? t('开') : t('关')}
      </label>
    )
  } else if (p.type === 'color') {
    const hex = typeof value === 'string' ? value : ''
    control = (
      <span className="flex items-center gap-2">
        <input
          type="color"
          aria-label={t('{title} 取色', { title: p.title ?? name })}
          data-testid={tid(`color-${name}`)}
          value={/^#[0-9a-fA-F]{6}$/.test(hex) ? hex : '#ffffff'}
          disabled={ro}
          onChange={(e) => ctx.set(e.target.value)}
          className="h-6 w-8 cursor-pointer rounded border border-line bg-panel-2 disabled:opacity-50"
        />
        <input
          type="text"
          aria-label={p.title ?? name}
          data-testid={tid(`input-${name}`)}
          value={hex}
          placeholder="#ffffff"
          disabled={ro}
          onChange={(e) => ctx.set(e.target.value.trim() === '' ? undefined : e.target.value)}
          className={`${base} w-24 font-mono`}
        />
      </span>
    )
  } else if (p.type === 'range') {
    const arr = Array.isArray(value) ? (value as unknown[]) : []
    const pair = (i: number) => (typeof arr[i] === 'number' ? (arr[i] as number) : undefined)
    const apply = (i: number, raw: string) => {
      const n = parseNum(raw)
      const next = [pair(0), pair(1)] as (number | undefined)[]
      next[i] = n
      if (next[0] == null && next[1] == null) return ctx.set(undefined)
      ctx.set([next[0] ?? p.min ?? 0, next[1] ?? p.max ?? 0])
    }
    control = (
      <span className="flex items-center gap-2">
        <input
          type="number"
          aria-label={t('{title} 下限', { title: p.title ?? name })}
          data-testid={tid(`input-${name}-0`)}
          min={p.min}
          max={p.max}
          step={p.step ?? 'any'}
          value={num(pair(0))}
          disabled={ro}
          onChange={(e) => apply(0, e.target.value)}
          className={`${base} w-20`}
        />
        <span className="text-[11px] text-ink-dim">~</span>
        <input
          type="number"
          aria-label={t('{title} 上限', { title: p.title ?? name })}
          data-testid={tid(`input-${name}-1`)}
          min={p.min}
          max={p.max}
          step={p.step ?? 'any'}
          value={num(pair(1))}
          disabled={ro}
          onChange={(e) => apply(1, e.target.value)}
          className={`${base} w-20`}
        />
        {(p.unit || p.advanced) && <span className="text-[11px] text-ink-dim">{p.unit ?? ''}</span>}
      </span>
    )
  } else if (p.type === 'asset_ref') {
    control = (
      <span className="flex flex-wrap items-center gap-2">
        <input
          type="text"
          aria-label={p.title ?? name}
          data-testid={tid(`input-${name}`)}
          value={typeof value === 'string' ? value : ''}
          placeholder={t('资产引用 id')}
          disabled={ro}
          onChange={(e) => ctx.set(e.target.value.trim() === '' ? undefined : e.target.value)}
          className={`${base} w-40 font-mono`}
        />
        <span className="text-[11px] text-ink-dim">
          {t('存资产引用，不存路径')}
          {(p.accept?.length ?? 0) > 0 ? t('（{list}）', { list: p.accept?.join(' / ') ?? '' }) : ''}
        </span>
      </span>
    )
  } else {
    // string（v1 不支持 pattern 即时校验：正则不在 schema 子集里）
    control = (
      <input
        type="text"
        aria-label={p.title ?? name}
        data-testid={tid(`input-${name}`)}
        value={typeof value === 'string' ? value : ''}
        placeholder={typeof p.default === 'string' ? p.default : '—'}
        disabled={ro}
        onChange={(e) => ctx.set(e.target.value === '' ? undefined : e.target.value)}
        className={`${base} w-56`}
      />
    )
  }

  const rangeHint =
    p.min != null && p.max != null
      ? t('{min} ~ {max}{step}', {
          min: p.min,
          max: p.max,
          step: p.step != null ? t(' · 步长 {n}', { n: p.step }) : '',
        })
      : p.options?.length
        ? t('可选 {opts}', { opts: p.options.join(' / ') })
        : ''
  const defaultHint = p.default !== undefined ? t('默认 {v}', { v: JSON.stringify(p.default) }) : t('无默认值')

  return (
    <div
      className="border-t border-line/60 py-1.5 first:border-t-0"
      data-testid={tid(`field-${name}`)}
      data-param-key={name}
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="w-32 shrink-0 text-[11px] text-ink" title={name}>
          {p.title ?? name}
          {required && (
            <span className="ml-1 text-rose-300" title={t('必填')}>
              *
            </span>
          )}
        </span>
        {control}
        {dirty && mode === 'edit' && (
          <button
            type="button"
            data-testid={tid(`reset-${name}`)}
            onClick={ctx.reset}
            className="text-[11px] text-ink-dim underline hover:text-accent"
            title={t('回到默认设置（这一项不再单独保存）')}
          >
            {t('恢复默认')}
          </button>
        )}
      </div>
      <div className="mt-0.5 flex flex-wrap items-center gap-x-2 pl-32 text-[10px] text-ink-dim">
        <span data-testid={tid(`hint-${name}`)}>
          {[defaultHint, rangeHint].filter(Boolean).join(' · ')}
        </span>
        {dirty && <span className="text-accent-soft">{t('已改（方案只保存这一项差异）')}</span>}
        {p.irreversible && (
          <span className="text-rose-300" data-testid={tid(`warn-irreversible-${name}`)}>
            {t('不可逆：执行后原状态回不去，提交时需二次确认')}
          </span>
        )}
        {p.affectsBody && (
          <span className="text-amber-300" data-testid={tid(`warn-affects-body-${name}`)}>
            {t('改图本体：只作用于工作版本，原图不动')}
          </span>
        )}
      </div>
    </div>
  )
}

export function ParamForm({ schema, value, onChange, mode = 'edit', testId }: ParamFormProps) {
  // 控件 testid 前缀：不传 = `param-*`（单实例场景）；传了 = `<testId>-*`
  // （插件详情页同时挂着「参数默认值」与「规格编辑」两份，必须能分开定位）
  const { t } = useI18n()
  const prefix = testId ?? 'param'
  const tid = (suffix: string) => `${prefix}-${suffix}`
  const props = useMemo(() => schema.properties ?? {}, [schema])
  const diff = value ?? EMPTY
  const eff = useMemo(() => effectiveOf(props, diff), [props, diff])
  const required = useMemo(() => new Set(schema.required ?? []), [schema])

  const set = (key: string, prop: ParamProperty, v: unknown) => {
    if (mode !== 'edit') return
    const next: Record<string, unknown> = { ...diff }
    // 改回默认值 = 不写 diff（规则 2：默认值唯一定义在插件里）
    if (v === undefined || sameValue(v, prop.default)) delete next[key]
    else next[key] = v
    onChange?.(stripHidden(schema, next))
  }

  const reset = (key: string) => {
    if (mode !== 'edit') return
    const next = { ...diff }
    delete next[key]
    onChange?.(stripHidden(schema, next))
  }

  // 分组：按 schema.groups 的顺序；`advanced` 或未归组的落到「高级」组
  const groups = useMemo(() => {
    const declared = (schema.groups ?? []).map((g) => ({ ...g }))
    const advKey = declared.find((g) => g.key === 'adv')?.key ?? 'adv'
    const out: { key: string; title: string; collapsed: boolean; keys: string[] }[] = declared.map(
      (g) => ({ key: g.key, title: g.title, collapsed: !!g.collapsed, keys: [] }),
    )
    const bucket = (key: string) => {
      let g = out.find((x) => x.key === key)
      if (!g) {
        g = { key, title: key === advKey ? t('高级') : key, collapsed: key === advKey, keys: [] }
        out.push(g)
      }
      return g
    }
    for (const [name, p] of Object.entries(props)) {
      const gk = p.advanced ? advKey : (p.group ?? declared[0]?.key ?? 'basic')
      bucket(gk).keys.push(name)
    }
    return out.filter((g) => g.keys.length > 0)
  }, [props, schema.groups, t])

  const hidden = (name: string) => !whenOk(props[name], eff)
  const body = (names: string[]) =>
    names
      .filter((n) => !hidden(n))
      .map((n) => (
        <Field
          key={n}
          ctx={{
            tid,
            name: n,
            p: props[n],
            mode,
            value: n in diff ? diff[n] : props[n].default,
            dirty: n in diff && !sameValue(diff[n], props[n].default),
            required: required.has(n),
            set: (v) => set(n, props[n], v),
            reset: () => reset(n),
          }}
        />
      ))

  const disabledNote =
    mode === 'disabled' ? (
      <p
        className="mb-2 rounded border border-amber-600/60 bg-amber-600/10 px-2 py-1 text-[11px] text-amber-300"
        data-testid={tid('disabled-note')}
      >
        {t('装好后可用：这个能力还没装上（控件灰着，参数照样看得见）。')}
      </p>
    ) : null

  const defaultsNote =
    mode === 'defaults' ? (
      <p className="mb-2 text-[11px] text-ink-dim" data-testid={tid('defaults-note')}>
        {t('只读：控件灰着不能改；想改参数，到「我的方案」另存一条自己的。')}
      </p>
    ) : null

  return (
    <div
      data-testid={prefix}
      aria-disabled={mode === 'disabled' || undefined}
      className={mode === 'disabled' ? 'opacity-70' : ''}
    >
      {disabledNote}
      {defaultsNote}
      {Object.keys(props).length === 0 ? (
        <p className="text-[11px] text-ink-dim" data-testid={tid('empty')}>
          {t('这个能力没有可配置参数（官方默认直接用，不画控件）。')}
        </p>
      ) : (
        groups.map((g) => (
          <section key={g.key} className="mt-1" data-testid={tid(`group-${g.key}`)}>
            {g.collapsed ? (
              <details>
                <summary className="cursor-pointer select-none text-[11px] font-medium text-ink-dim">
                  {t('{title}（{n} 项，默认收起）', {
                    title: g.title,
                    n: g.keys.filter((k) => !hidden(k)).length,
                  })}
                </summary>
                <div className="mt-1">{body(g.keys)}</div>
              </details>
            ) : (
              <>
                <h4 className="text-[11px] font-medium text-ink-dim">{g.title}</h4>
                <div className="mt-1">{body(g.keys)}</div>
              </>
            )}
          </section>
        ))
      )}
    </div>
  )
}

import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react'
import { api, type TagCandidateItem } from '../lib/api'
import { normalizeTagQuery } from '../lib/searchQuery'
import { useI18n } from '../i18n'

/**
 * 统一标签输入（task / KI-13 / R40）：受控 input + 候选面板，替换此前散落在
 * 图库侧栏、搜索页、检查器、批量打标对话框的裸 input（同一能力只有一份实现）。
 *
 * 契约：
 *  - 输入即查：≥1 字符触发，250ms 防抖，`/api/library/tags?prefix=` 前缀候选（服务端范围扫描）；
 *  - 候选按 key（值里第一个 `-` 前段）分组展示 `值 × 计数`，裸值进「（未分组）」；
 *  - 键盘可达（ARIA combobox/listbox）：↑↓ 移动高亮、Enter 确认、Esc 关闭；
 *  - 场景区分（R35「回车即新建」不许在筛选场景发生）：
 *      scenario='filter' → 无匹配只给「无匹配标签」，不新建；只输入 key 时提示「请选择具体值」；
 *      scenario='tag'    → 无匹配给「回车将新建「输入」」，Enter 把原输入交给 onCommit；
 *  - 手输完整值仍精确命中：选中候选后 value 就是完整值，筛选/打标都用它。
 */

export interface TagPickerProps {
  /** 受控输入值（完整标签值或输入中的前缀） */
  value: string
  onChange: (v: string) => void
  /** 选中候选（点选或 Enter 确认高亮项）：回调里拿到完整 value */
  onPick: (value: string) => void
  /** filter=筛选（不新建）；tag=打标（无匹配回车新建） */
  scenario: 'filter' | 'tag'
  /** Enter 且无高亮候选时把原输入交出去（tag 场景 = 新建标签；filter 场景不传） */
  onCommit?: (raw: string) => void
  /** 输入框 testid 基名：面板/提示/空态/候选项 testid 由它派生 */
  testid: string
  /** 候选面板 testid 基名（默认 = testid）：面板 `${suggestTestid}-suggest` */
  suggestTestid?: string
  placeholder?: string
  /** 输入框样式（宽度等）；面板样式固定 */
  className?: string
  disabled?: boolean
  /** 候选条数上限（默认 30，服务端夹取 200） */
  limit?: number
  /** 防抖毫秒（默认 250） */
  debounceMs?: number
  /** 面板最大高度类（批量打标对话框里矮一些） */
  panelClassName?: string
}

/** 组身份用中文原文；展示点 t() 翻译。 */
const UNGROUPED = '（未分组）'

/** 候选按 key 分组（服务端给 key；缺字段时按第一个 `-` 回退），保持服务端次序。 */
export function groupCandidates(items: TagCandidateItem[]): [string, TagCandidateItem[]][] {
  const map = new Map<string, TagCandidateItem[]>()
  for (const it of items) {
    const i = it.value.indexOf('-')
    const g = it.key ?? (i > 0 ? it.value.slice(0, i) : UNGROUPED)
    const list = map.get(g)
    if (list) list.push(it)
    else map.set(g, [it])
  }
  return [...map.entries()]
}

export function TagPicker({
  value,
  onChange,
  onPick,
  scenario,
  onCommit,
  testid,
  suggestTestid,
  placeholder,
  className,
  disabled,
  limit = 30,
  debounceMs = 250,
  panelClassName,
}: TagPickerProps) {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const [items, setItems] = useState<TagCandidateItem[] | null>(null)
  const [total, setTotal] = useState(0)
  /** 高亮序（-1 = 未导航；↑↓ 从 -1 起走到 0 / 末项） */
  const [hi, setHi] = useState(-1)
  const blurTimer = useRef<number | null>(null)
  const alive = useRef(true)
  const seq = useRef(0)

  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
      if (blurTimer.current !== null) clearTimeout(blurTimer.current)
    }
  }, [])

  const prefix = normalizeTagQuery(value)
  /** Enter 提交用原文（trim）：逗号分隔的多值不能被整体归一化（`雪山, 日出` → `雪山,_日出`） */
  const rawInput = value.trim()
  // 输入即查（防抖）；清空 → 面板收起、候选置空
  useEffect(() => {
    if (!open) return
    if (!prefix) {
      setItems(null)
      setTotal(0)
      return
    }
    const my = ++seq.current
    const t = window.setTimeout(() => {
      api
        .tagCandidates(prefix, { limit })
        .then((r) => {
          if (!alive.current || my !== seq.current) return // 过期响应丢弃（快速敲字不乱序）
          setItems(r.items)
          setTotal(r.total)
          setHi(-1) // 新候选 = 无高亮（↑↓ 才落位，避免回车误选第一项）
        })
        .catch(() => {
          if (!alive.current || my !== seq.current) return
          setItems([])
          setTotal(0)
        })
    }, debounceMs)
    return () => clearTimeout(t)
  }, [prefix, open, limit, debounceMs])

  const groups = useMemo(() => (items ? groupCandidates(items) : []), [items])
  /** 高亮项在扁平列表中的序号 → 候选（分组展示但导航是扁平序） */
  const flat = useMemo(() => items ?? [], [items])

  const close = useCallback(() => {
    setOpen(false)
    setHi(-1)
  }, [])

  const pick = useCallback(
    (v: string) => {
      if (blurTimer.current !== null) {
        clearTimeout(blurTimer.current)
        blurTimer.current = null
      }
      close()
      onPick(v)
    },
    [close, onPick],
  )

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      // 只归输入框：面板开着先close，没开也吞掉（不冒泡去关外层抽屉）
      e.stopPropagation()
      close()
      return
    }
    if (e.key === 'ArrowDown') {
      if (!open) {
        setOpen(true)
        return
      }
      if (flat.length === 0) return
      e.preventDefault()
      setHi((i) => (i + 1) % flat.length)
      return
    }
    if (e.key === 'ArrowUp') {
      if (!open || flat.length === 0) return
      e.preventDefault()
      setHi((i) => (i <= 0 ? flat.length - 1 : i - 1))
      return
    }
    if (e.key === 'Enter') {
      e.preventDefault()
      const hit = open && hi >= 0 ? flat[hi] : undefined
      if (hit) {
        pick(hit.value)
        return
      }
      // 未导航时：手输的值与某个候选完全相同 → 当作选中它（「手输全值仍精确命中」）
      const exact = rawInput ? flat.find((it) => it.value === rawInput) : undefined
      if (exact) {
        pick(exact.value)
        return
      }
      // 无匹配：打标场景回车 = 新建（filter 场景不新建）；传原文，多值由调用方切分
      if (scenario === 'tag' && rawInput) onCommit?.(rawInput)
    }
  }

  const showPanel = open && prefix !== ''
  const loading = items === null
  /** 面板系 testid 基名：Search 页沿用历史 `tag-suggest*`（既有验收脚本/单测钉的契约） */
  const sid = suggestTestid ?? testid

  return (
    <div className="relative">
      <input
        data-testid={testid}
        value={value}
        disabled={disabled}
        onChange={(e) => {
          onChange(e.target.value)
          setOpen(true)
        }}
        onFocus={() => setOpen(true)}
        onBlur={() => {
          // 延迟关闭：给点选候选留出 mousedown 窗口
          blurTimer.current = window.setTimeout(() => setOpen(false), 150)
        }}
        onKeyDown={onKeyDown}
        placeholder={placeholder}
        spellCheck={false}
        role="combobox"
        aria-expanded={showPanel}
        aria-autocomplete="list"
        aria-controls={`${sid}-listbox`}
        className={className}
      />
      {showPanel && (
        <div
          data-testid={`${sid}-suggest`}
          id={`${sid}-listbox`}
          role="listbox"
          aria-label={t('标签候选')}
          className={
            panelClassName ??
            'absolute top-full left-0 z-40 mt-1 max-h-72 w-72 overflow-y-auto rounded-md border border-line bg-panel p-1 text-xs shadow-lg'
          }
        >
          {/* 只输入了 key（无 `-`）且确有候选：提示要选具体值，筛选才精确 */}
          {scenario === 'filter' && !prefix.includes('-') && items !== null && items.length > 0 && (
            <p className="px-2 py-1 text-ink-dim" data-testid={`${sid}-suggest-hint`}>
              {t('只输入了 key：请选择具体值（完整值才能精确过滤）')}
            </p>
          )}
          {loading ? (
            <p className="px-2 py-1 text-ink-dim" data-testid={`${sid}-suggest-loading`}>
              {t('候选加载中…')}
            </p>
          ) : flat.length === 0 ? (
            <p className="px-2 py-1 text-ink-dim" data-testid={`${sid}-suggest-empty`}>
              {scenario === 'tag' && rawInput ? (
                <span data-testid={`${sid}-suggest-create`}>{t('无匹配标签，回车将新建「{value}」', { value: rawInput })}</span>
              ) : (
                t('无匹配标签')
              )}
            </p>
          ) : (
            groups.map(([g, list]) => (
              <div key={g} data-testid={`${sid}-suggest-group-${g}`}>
                <p className="px-2 pt-1.5 pb-0.5 text-[11px] font-medium text-ink-dim">{g === UNGROUPED ? t(UNGROUPED) : g}</p>
                {list.map((it) => {
                  const idx = flat.indexOf(it)
                  return (
                    <div
                      key={it.value}
                      role="option"
                      aria-selected={idx === hi}
                      data-testid={`${sid}-suggest-item-${it.value}`}
                      className={`flex w-full cursor-pointer items-center justify-between gap-2 rounded px-2 py-1 text-left ${
                        idx === hi ? 'bg-panel-2' : 'hover:bg-panel-2'
                      }`}
                      onMouseDown={(e) => {
                        // mousedown 先于 blur：保证点选一定先于关闭生效
                        e.preventDefault()
                        pick(it.value)
                      }}
                      onMouseEnter={() => setHi(idx)}
                    >
                      <span className="truncate text-ink">{it.value}</span>
                      <span className="shrink-0 text-ink-dim">{it.count}</span>
                    </div>
                  )
                })}
              </div>
            ))
          )}
          {items !== null && total > items.length && (
            <p className="px-2 py-1 text-ink-dim" data-testid={`${sid}-suggest-more`}>
              {t('仅显示前 {shown} 个（共 {total} 个）', { shown: items.length, total })}
            </p>
          )}
        </div>
      )}
    </div>
  )
}

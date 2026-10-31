/**
 * 统一标签裁决（2026-10-19）的前端配套：值数组输出、多词 `_` 连接、key-val 前缀分组。
 * 分组只是**展示层**：筛选/写库/删除仍按完整 value 精确匹配（`尺寸-滚筒_10KG`），
 * 不改后端契约（tag 表只有 value 列，uq(assetId,value)）。
 */

/** 新建自定义打标任务时预填 / 「插入格式要求」按钮写入的输出格式段 */
export const TAG_PROMPT_FORMAT_CLAUSE =
  '输出 JSON 字符串数组，每个元素是一个离散标签，值必须自解释；多词用下划线连接（如 MacBook_Pro_13寸）；不要空格、不要标点、不要解释；没有内容输出 []。'

/**
 * prompt 是否声明了输出格式：须同时含「数组」与（`_` 或「下划线」）。
 * 缺任一项 → 保存前弹确认「未声明输出格式，标签将无法自解释」，默认阻止，确认后才放行。
 */
export function promptDeclaresOutputFormat(prompt: string): boolean {
  const p = prompt ?? ''
  return p.includes('数组') && (p.includes('_') || p.includes('下划线'))
}

/** key-val 拆分：第一个 `-` 前为组（key = 任务名），其余为组内值；无 `-` → 裸值（key=''） */
export function splitTagKeyValue(value: string): { key: string; val: string } {
  const i = value.indexOf('-')
  if (i <= 0) return { key: '', val: value }
  return { key: value.slice(0, i), val: value.slice(i + 1) }
}

export interface TagDisplayGroup<T> {
  /** 组前缀（key）；'' = 无前缀裸值组（平铺展示，不渲染组标题） */
  key: string
  entries: {
    /** 原始记录（TagRecord / TagCount） */
    item: T
    /** 完整 value —— 筛选 / 删除 / 拖放打标仍用它 */
    value: string
    /** 组内展示值（去掉 `key-` 前缀；裸值组 = 原值） */
    display: string
  }[]
}

/**
 * 按第一个 `-` 的前缀分组（仅展示）：`尺寸-滚筒_10KG` → 组「尺寸」内值 `滚筒_10KG`；
 * 无 `-` 的裸值进 `key=''` 平铺组。裸值组排最前，其余按组第一次出现的顺序。
 */
export function groupByTagPrefix<T>(items: T[], valueOf: (item: T) => string): TagDisplayGroup<T>[] {
  const plain: TagDisplayGroup<T>['entries'] = []
  const groups = new Map<string, TagDisplayGroup<T>['entries']>()
  for (const item of items) {
    const value = valueOf(item)
    const { key, val } = splitTagKeyValue(value)
    if (!key) {
      plain.push({ item, value, display: val })
      continue
    }
    const list = groups.get(key)
    if (list) list.push({ item, value, display: val })
    else groups.set(key, [{ item, value, display: val }])
  }
  const out: TagDisplayGroup<T>[] = []
  if (plain.length > 0) out.push({ key: '', entries: plain })
  for (const [key, entries] of groups) out.push({ key, entries })
  return out
}

/**
 * 打标结果（tag_result.value = 模型输出原文 / 数组快照 JSON）→ 展示值列表。
 * 值是 JSON 字符串数组（统一裁决后的快照格式）→ 逐项返回；否则返回 null（按原文展示）。
 * 解析失败 / 混入非字符串元素一律按原文展示，不猜测。
 */
export function parseTagResultValues(value: string): string[] | null {
  const v = (value ?? '').trim()
  if (!v.startsWith('[') || !v.endsWith(']')) return null
  try {
    const parsed: unknown = JSON.parse(v)
    if (!Array.isArray(parsed)) return null
    if (!parsed.every((x) => typeof x === 'string')) return null
    return parsed as string[]
  } catch {
    return null
  }
}

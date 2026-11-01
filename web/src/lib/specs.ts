/**
 * N12：方案（用户嘴里的「方案」）在界面上的**唯一分类与选择逻辑**（纯函数，可 node 直跑）。
 *
 * 三条口径（对应 `references/plugin-design-and-ui.md` §3 用户视角两动词三名词）：
 *
 *  1. **一个能力只有两个动词**：`check`（检查这批图能不能用）/ `process`（把图变成我要的样子）。
 *     方案属于哪个动词**由方案数据本身决定**：有动作链 = 处理，没有动作链 = 检查。
 *     不从插件目录、不从界面硬编码插件 id 猜 —— 换行业包/换插件时不用改前端。
 *  2. **只有一条就默认选中**（用户少点一次）：`pickDefaultSpec` 是唯一入口。
 *  3. **界面只认「方案 / 结果」**，不出现插件、任务号、判定内部词（见 `scripts/ui_copy_guard.py`）。
 */
import type { SpecActionView, SpecView } from './api'
import { tGlobal } from '../i18n'

export type Verb = 'check' | 'artifact'

/** 原文键常量（i18n-D 过渡期保留，消费方直读；locale 感知取词走 verbLabel()） */
export const VERB_LABEL: Record<Verb, string> = {
  check: '检查',
  artifact: '处理',
}

/** i18n-D：动词标签按当前 locale 取词 */
export function verbLabel(v: Verb): string {
  return tGlobal(VERB_LABEL[v])
}

/** 该方案属于哪个动词：有动作链 = 处理；只有规则/阈值 = 检查 */
export function specVerb(spec: Pick<SpecView, 'actions'>): Verb {
  return (spec.actions?.length ?? 0) > 0 ? 'artifact' : 'check'
}

/** 动词下可用的方案（停用的不列：它不能被任务引用） */
export function specsForVerb(list: readonly SpecView[], verb: Verb): SpecView[] {
  return list.filter((s) => s.enabled !== 0 && specVerb(s) === verb)
}

/**
 * 默认选中的方案：
 * - 传了 `wanted`（深链/上次选择）且仍可用 → 用它；
 * - 否则该动词下**只有一条** → 直接选它（不让用户再点一次）；
 * - 否则 null（用户必须显式选，避免「默认用了哪条说不清」）。
 */
export function pickDefaultSpec(
  list: readonly SpecView[],
  verb: Verb,
  wanted?: number | null,
): SpecView | null {
  const usable = specsForVerb(list, verb)
  if (wanted != null) {
    const hit = usable.find((s) => s.specId === wanted)
    if (hit) return hit
  }
  return usable.length === 1 ? usable[0] : null
}

/** 动作链一句话（处理页展示「这条方案会做哪几步」；顺序就是执行顺序） */
export function actionChainText(spec: Pick<SpecView, 'actions'>): string {
  const names = (spec.actions ?? []).map((a) => a.action).filter(Boolean)
  return names.length > 0 ? names.join(' → ') : tGlobal('（无动作链）')
}

/** 方案副标题：检查类看规则数，处理类看动作链 */
export function specSubtitle(spec: SpecView): string {
  if (specVerb(spec) === 'artifact') return actionChainText(spec)
  const n = spec.rules?.length ?? 0
  return n > 0 ? tGlobal('{n} 条规则', { n }) : tGlobal('全部规则')
}

/**
 * 方案当前能不能跑（环境事实来自 `pluginStatus`，不猜）：
 * 停用 / 能力未安装 / 版本漂移 → 不能跑，并且**说清原因**（灰置 + 原因，不隐藏）。
 */
export function specRunBlockedReason(spec: SpecView | null): string | null {
  if (!spec) return tGlobal('先选一条方案')
  if (spec.enabled === 0) return tGlobal('这条方案已停用（到设置里启用后再用）')
  if (!spec.pluginStatus.installed) {
    return spec.pluginStatus.note ?? tGlobal('这条方案依赖的能力未安装')
  }
  if (spec.pluginStatus.versionMismatch) {
    return tGlobal('方案记录的能力版本与已装版本不一致（已装 {v}），到设置里核对后再跑', {
      v: spec.pluginStatus.version ?? tGlobal('未知'),
    })
  }
  return null
}

/** 图集（作用范围）——两个动词共用同一套说法 */
export type SetKind = 'selection' | 'pending' | 'all'

export interface AssetSetOption {
  kind: SetKind
  label: string
  count?: number
}

/**
 * 可选图集：处理必须明确「处理哪一批」（不给「全部」兜底，避免一键改全库）；
 * 检查额外允许「还没检查过的」与「全部」。
 */
export function assetSetOptions(verb: Verb, selected: number, pending?: number, total?: number): AssetSetOption[] {
  const opts: AssetSetOption[] = [
    {
      kind: 'selection',
      label:
        selected > 0
          ? tGlobal('选中的 {n} 张', { n: selected })
          : tGlobal('选中的 0 张（先在图库选图）'),
      count: selected,
    },
  ]
  if (verb === 'check') {
    opts.push({ kind: 'pending', label: tGlobal('还没检查过的图'), count: pending })
    opts.push({ kind: 'all', label: tGlobal('全部图库'), count: total })
  }
  return opts
}

/** 解析 URL 里的 `ids=1,2,3`（图库/搜索页把选择集带过来） */
export function parseIds(raw: string | null | undefined): number[] {
  if (!raw) return []
  const out: number[] = []
  for (const part of raw.split(',')) {
    const n = Number(part.trim())
    if (Number.isInteger(n) && n > 0 && !out.includes(n)) out.push(n)
  }
  return out
}

/** 处理类的执行体：动作链原样上交（宿主按方案执行，前端不改写参数） */
export function processRequestOf(spec: SpecView, assetIds: number[]): { specId: number; assetIds: number[] } {
  return { specId: spec.specId, assetIds }
}

/** 动作链里出现危险动作（改本体/不可逆）→ 界面必须二次确认 */
export const IRREVERSIBLE_ACTIONS = ['watermark', 'inpaint', 'white_bg_fill', 'crop', 'resize', 'compress', 'convert']

export function chainAffectsBody(spec: Pick<SpecView, 'actions'>): boolean {
  return (spec.actions ?? []).some((a: SpecActionView) => IRREVERSIBLE_ACTIONS.includes(a.action))
}

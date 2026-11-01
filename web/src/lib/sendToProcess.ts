/**
 * KI35-P3（t_06bb84bd）：选择集「送去处理」能力面板的**纯逻辑**（node 直跑，可单测）。
 *
 * 三件事：
 *  1. 能力的动词词：判定/处理由 **manifest outputs** 决定（judge=检查「这些图能不能用」，
 *     process=处理「把图变成我要的样子」）—— 不从目录猜、不在界面硬编码插件 id；
 *  2. 方案分组：官方默认（包里带来的，`originKey` 非空） vs 我的方案（用户自建）；
 *  3. 范围选项：当前选中 / 当前筛选结果 / 全部图库 —— 全部只给**纯判定**能力
 *     （处理类一键改全库是 specs.ts 一贯的红线：处理必须明确「处理哪一批」）。
 *
 * 文案红线（同 P1/P2）：能力/方案两个名词，不出现 插件/规格/任务/job/verdict/manifest/行业包。
 */
import type { ExportFilterPayload, SpecView } from './api'
import { capabilityPath } from './capabilityNav'
import type { CapabilityNavItem } from './capabilityNav'
import type { PluginOutput } from './pluginResult'
import { specVerb } from './specs'
import { tGlobal } from '../i18n'

/** 作用范围（与导出 Dialog 的「选择集 / 当前筛选」同一套口径，多一档「全部」） */
export type ProcessRange = 'selection' | 'filter' | 'all'

/** 「当前筛选结果」寄存体（与 lib/export.ts 的 ExportFilterScope 同形：label + filter 同一份对象） */
export interface ProcessFilterScope {
  label: string
  filter: ExportFilterPayload
}

// ---------------------------------------------------------------------------------------
// 「当前筛选结果」范围上下文寄存（载体与理由同 lib/export.ts：sessionStorage，
// F5 后仍在、关标签页即弃；寄存失败只是少这一个范围选项，不阻塞选择集）
// ---------------------------------------------------------------------------------------

const FILTER_SCOPE_KEY = 'dam.process.filterScope'

export function stashProcessFilterScope(scope: ProcessFilterScope | null): void {
  try {
    if (!scope) sessionStorage.removeItem(FILTER_SCOPE_KEY)
    else sessionStorage.setItem(FILTER_SCOPE_KEY, JSON.stringify(scope))
  } catch {
    // sessionStorage 不可用：静默降级为「只有选择集 / 全部」范围
  }
}

export function loadProcessFilterScope(): ProcessFilterScope | null {
  try {
    const raw = sessionStorage.getItem(FILTER_SCOPE_KEY)
    if (!raw) return null
    const v = JSON.parse(raw) as ProcessFilterScope | null
    if (!v || typeof v !== 'object' || typeof v.label !== 'string' || typeof v.filter !== 'object') {
      return null
    }
    return v
  } catch {
    return null
  }
}

/**
 * 能力的动词词（manifest outputs → 用户嘴里的动词）。
 * 两个都占（judge+process）显示「检查 + 处理」；闭集外/缺声明按后端 kinds_json 同款缺省
 * （判定）处理 —— 与 `crates/app-web/src/compliance_api.rs::kinds_json` 的兜底一致。
 */
export function capabilityVerbWord(outputs: readonly PluginOutput[] | null | undefined): string {
  const ks = outputs ?? []
  const judge = ks.includes('verdict')
  const process = ks.includes('artifact')
  if (judge && process) return tGlobal('检查 + 处理')
  if (process) return tGlobal('处理')
  return tGlobal('检查')
}

/** 「全部图库」只给纯判定能力：处理类（含 judge+process 双占）不给一键改全库的兜底 */
export function allowAllRange(outputs: readonly PluginOutput[] | null | undefined): boolean {
  const ks = outputs ?? []
  return ks.includes('verdict') && !ks.includes('artifact')
}

/**
 * 方案分组：官方默认（`originKey` 非空 = 包里带来的/迁移来的预设） vs 我的方案（自建，originKey 空）。
 * 停用的（enabled=0）不列 —— 不能被执行的方案出现在选择列表里只会误导。
 */
export interface PlanGroups {
  official: SpecView[]
  mine: SpecView[]
}

export function planGroups(specs: readonly SpecView[]): PlanGroups {
  const usable = specs.filter((s) => s.enabled !== 0)
  return {
    official: usable.filter((s) => s.originKey != null),
    mine: usable.filter((s) => s.originKey == null),
  }
}

/** 该能力下可执行的方案总数（分组只是展示，默认选中判据看总数） */
export function usablePlanCount(g: PlanGroups): number {
  return g.official.length + g.mine.length
}

export interface RangeOption {
  kind: ProcessRange
  label: string
  /** 已知命中数（筛选范围读数未回来时为 null，显示 …） */
  count: number | null
}

/**
 * 范围选项（纯函数）：当前选中永远有；筛选结果只在页面寄存了上下文时出现；
 * 全部图库只对纯判定能力出现。顺序 = 用户心里的优先级（选中 > 筛选 > 全部）。
 */
export function rangeOptions(args: {
  selected: number
  filterScope: ProcessFilterScope | null
  filterTotal: number | null
  outputs: readonly PluginOutput[] | null | undefined
}): RangeOption[] {
  const opts: RangeOption[] = [
    {
      kind: 'selection',
      label:
        args.selected > 0
          ? tGlobal('当前选中的 {n} 张', { n: args.selected })
          : tGlobal('当前选中的 0 张（先在图库选图）'),
      count: args.selected,
    },
  ]
  if (args.filterScope) {
    opts.push({
      kind: 'filter',
      label: tGlobal('当前筛选结果（{label}）', { label: args.filterScope.label }),
      count: args.filterTotal,
    })
  }
  if (allowAllRange(args.outputs)) {
    opts.push({ kind: 'all', label: tGlobal('全部图库（已删的不算）'), count: null })
  }
  return opts
}

/** 筛选范围回读成执行集合的上限：超过就拒发（让用户缩小范围/分批，不做静默截断） */
export const FILTER_IDS_CAP = 5000

/**
 * 能力面板的能力清单 = 注册表（packs）∪ 方案清单里"能力在装但注册表还没有行"的补集。
 *
 * 为什么需要补集（实测 reports/evidence/t_06bb84bd/）：内置判定包第一次被跑判定之前**不进
 * 注册表**（`plugins[]` 为空，账本行由判定/装包路径惰性写）—— fresh 库上图库选图打开面板
 * 会是空的，但 GET /api/specs 已经能看到它 seeding 出的官方默认方案（pluginStatus.installed
 * = 后端实算的装载态）。用一个**只增不覆盖**的并集，注册表里的行（outputs 来自 manifest）
 * 永远优先，补集行的 outputs 由方案数据本身派生（有动作链 = 处理）。
 */
export function specsDerivedCapabilities(
  specs: readonly SpecView[],
  knownIds: ReadonlySet<string>,
): CapabilityNavItem[] {
  const byPlugin = new Map<string, CapabilityNavItem>()
  for (const s of specs) {
    if (s.enabled === 0 || !s.pluginStatus.installed) continue
    if (knownIds.has(s.pluginId)) continue
    const cur = byPlugin.get(s.pluginId)
    if (!cur) {
      // 名字/描述同样**接口下发**（DM-B1：`pluginStatus.pluginName/pluginSummary` 来自包
      // 自己的 manifest；前端没有 id → 名字 映射表）。真拿不到才退 id。
      const label = s.pluginStatus.pluginName?.trim()
      byPlugin.set(s.pluginId, {
        id: s.pluginId,
        to: capabilityPath(s.pluginId),
        label: label && label.length > 0 ? label : s.pluginId,
        blurb: s.pluginStatus.pluginSummary?.trim() ?? '',
        outputs: specVerb(s) === 'artifact' ? ['artifact'] : ['verdict'],
      })
    } else if (specVerb(s) === 'artifact' && !cur.outputs.includes('artifact')) {
      cur.outputs = [...cur.outputs, 'artifact']
    }
  }
  return [...byPlugin.values()].sort((a, b) => a.id.localeCompare(b.id))
}

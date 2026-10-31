/**
 * 插件**展示语义**的唯一实现（PackManager 列表页与插件详情页共用）。
 *
 * 为什么抽出来：同一句「装载态 可用 / 缺包 / 包损坏」、同一套签名词、同一个版本排序，
 * 若在列表页与详情页各写一份，改一处必然漏另一处 —— 用户会看到两个页面说不同的话。
 *
 * 语义边界（别在界面层替后端猜）：
 * - `runtime` = **静态装载态**（包目录在不在 + manifest 解析得开），后端不拉子进程；
 * - 子进程的 running / 冷却（300s）/ 崩溃属**判定时刻**的运行态，静态快照拿不到，
 *   失败会以「未判定 + plugin_timeout / plugin_crash / plugin_unavailable」出现在任务中心这次执行的明细里。
 */
import type {
  ComplianceInstalledPluginPack,
  CompliancePluginState,
  CompliancePluginVersionRow,
  ComplianceSignatureStatus,
} from './api'
import type { PluginKind } from './pluginResult'
import { tGlobal } from '../i18n'
import type { PackText } from '../i18n/pickI18n'

/** 签名状态 → 人话（migration 0026 的 signature_status 闭集，词不由界面发明） */
const SIG_LABELS: Record<ComplianceSignatureStatus, string> = {
  unsigned: '未签名',
  declared: '有签名·未验签',
  verified: '验签通过',
  invalid: '签名无效',
}

export function sigLabel(s: ComplianceSignatureStatus | null | undefined): string {
  if (!s) return '—'
  // i18n-D：原文键查表 + tGlobal 取词；闭集外的原样透出
  return SIG_LABELS[s] ? tGlobal(SIG_LABELS[s]) : s
}

export function sigClass(s: ComplianceSignatureStatus | null | undefined): string {
  if (s === 'verified') return 'border-emerald-600/60 bg-emerald-600/10 text-emerald-300'
  if (s === 'invalid') return 'border-rose-600/60 bg-rose-600/10 text-rose-300'
  if (s === 'declared') return 'border-amber-600/60 bg-amber-600/10 text-amber-300'
  return 'border-line text-ink-dim'
}

/** 插件种类 → 用户嘴里的动作词（界面禁用词「插件/job/verdict」）。 */
export function kindWord(kinds: PluginKind[] | null | undefined): string {
  const ks = kinds ?? ['judge']
  if (ks.includes('judge') && ks.includes('process')) return tGlobal('检查 + 处理')
  return ks.includes('process') ? tGlobal('处理') : tGlobal('检查')
}

/** 装载态（`runtime`）→ 人话 + 颜色 + 完整解释（title 用）。 */
export function runtimeOf(rt: ComplianceInstalledPluginPack['runtime']): {
  word: string
  cls: string
  hint: string
} {
  if (rt.available) {
    return {
      word: tGlobal('可用'),
      cls: 'border-emerald-600/60 bg-emerald-600/10 text-emerald-300',
      hint:
        (rt.version
          ? tGlobal('manifest 就绪（{v}）；判定时才拉起子进程。', { v: rt.version })
          : tGlobal('manifest 就绪；判定时才拉起子进程。')) +
        tGlobal('子进程的 running / 冷却 / 崩溃属判定时刻的运行态，不在此静态快照内。'),
    }
  }
  if (rt.reason === 'plugin_pack_missing') {
    return {
      word: tGlobal('缺包'),
      cls: 'border-rose-600/60 bg-rose-600/10 text-rose-300',
      hint: rt.note ?? tGlobal('未找到包目录：先装包，再切到插件引擎'),
    }
  }
  if (rt.reason === 'plugin_manifest_invalid') {
    return {
      word: tGlobal('包损坏'),
      cls: 'border-rose-600/60 bg-rose-600/10 text-rose-300',
      hint: rt.note ? tGlobal('manifest 解析失败：{note}', { note: rt.note }) : tGlobal('manifest 解析失败：未知原因'),
    }
  }
  return {
    word: tGlobal('不可用'),
    cls: 'border-rose-600/60 bg-rose-600/10 text-rose-300',
    hint: rt.note ?? rt.reason ?? tGlobal('不可用（后端未给原因）'),
  }
}

/** 包内按 version 从新到旧排（界面顺序稳定，测试与用户预期一致） */
export function sortVersions(rows: CompliancePluginVersionRow[]): CompliancePluginVersionRow[] {
  return [...rows].sort((a, b) => {
    const ai = a.installedAt ?? 0
    const bi = b.installedAt ?? 0
    if (ai !== bi) return bi - ai
    return b.version.localeCompare(a.version)
  })
}

/** 字节数 → 人话体积（宿主没上报体积时给 `—`，不拿 0 冒充） */
export function fmtBytes(n: number | null | undefined): string {
  if (n == null || !Number.isFinite(n) || n <= 0) return '—'
  if (n >= 1024 * 1024 * 1024) return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`
  if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`
  if (n >= 1024) return `${Math.round(n / 1024)} KB`
  return `${n} B`
}

/**
 * 一个插件的**事实汇总**：账本（`/api/compliance/packs.plugins`）+ 内置包身份
 * （`packs.builtin[].plugin`）两个来源合并。
 *
 * 为什么必须合并（踩过的坑）：内置包随产品分发，**不进** `packs.plugins` 账本
 * ——只看账本会把「仓库自带的电商包」显示成「未安装」，用户去点「装包」永远装不上。
 * 账本有行 = 有版本栈/可切可卸；只有内置身份 = 可跑、可看版本、但没有多版本栈。
 */
export interface ResolvedPlugin {
  id: string
  /** 能力名（DM-B1：包自己给；拿不到 = null，界面退显 id） */
  name: string | null
  /** 名字的原样 i18n 对象（i18n-F；包写对象形态时才有，前端 pickI18n 切语言即时生效） */
  nameI18n: PackText
  /** 一句话「做什么 + 有什么用」（manifest `summary`；未声明 = null） */
  summary: string | null
  /** 描述的原样 i18n 对象（i18n-F；同 nameI18n 口径） */
  summaryI18n: PackText
  /** 任一来得及：账本行 or 内置身份 */
  found: boolean
  builtin: boolean
  currentVersion: string | null
  staleCurrent: string | null
  versions: CompliancePluginVersionRow[]
  /** 与 `/status.pluginState` 同语义的静态装载态（只读 manifest） */
  runtime: CompliancePluginState | null
  /** 包解析错误（manifest 坏 / 缺包），界面必须显示 */
  error: string | null
}

/**
 * IP-K16 之后**只有一个来源**：`/api/compliance/packs.plugins`（账本 + 已装版本目录）。
 * 旧的「内置 YAML 行业包身份」那一族随 K16 一起删了，界面不再合并两族数据 ——
 * 内置包照样随产品分发，但它现在就是账本里的一行（`builtin: true`）。
 */
interface PacksLike {
  plugins?: ComplianceInstalledPluginPack[]
}

export function resolvePlugin(packs: PacksLike, pluginId: string): ResolvedPlugin {
  const row = (packs.plugins ?? []).find((p) => p.id === pluginId)
  if (!row) {
    return {
      id: pluginId,
      name: null,
      nameI18n: null,
      summary: null,
      summaryI18n: null,
      found: false,
      builtin: false,
      currentVersion: null,
      staleCurrent: null,
      versions: [],
      runtime: null,
      error: null,
    }
  }
  return {
    id: row.id,
    name: row.name ?? null,
    nameI18n: row.nameI18n ?? null,
    summary: row.summary ?? null,
    summaryI18n: row.summaryI18n ?? null,
    found: true,
    builtin: row.builtin,
    currentVersion: row.currentVersion,
    staleCurrent: row.staleCurrent,
    versions: sortVersions(row.versions),
    runtime: row.runtime,
    error: null,
  }
}

/** 插件清单（列表页）：账本行按 id 排 */
export function pluginList(packs: PacksLike): ResolvedPlugin[] {
  return (packs.plugins ?? [])
    .map((p) => p.id)
    .sort()
    .map((id) => resolvePlugin(packs, id))
}

/**
 * KI-35 P1（task）+ DM-B1（task）：主导航「处理」区由**已装包注册表**驱动。
 *
 * 设计口径（plugin-design-and-ui.md §8 第 10 条）：主导航不得硬编码任何第二档能力的
 * tab —— 处理区导航项 = 已安装且装载可用的能力，装了出现、卸载消失；打标任务
 * （第一档内置功能）的 tab 不在此列。
 *
 * 名字从哪来（DM-B1 / KI-36 起）：**接口下发，包自己给**。
 * `GET /api/compliance/packs` 的 `plugins[]` 行带 `name`（manifest `short_name`，老包回落
 * `name`）与 `summary`（一句话「做什么 + 有什么用」）—— 本文件**没有** id → 名字的映射表
 * （原 id → 名字的硬编码映射表已随 KI-36 删除：装了映射外的能力时它只能退显裸 id）。
 * 真的拿不到名字（包目录不在 / manifest 不可解析）才退显 id —— 缺口一眼可见，不假装有名字。
 */
import { useEffect, useState } from 'react'

import { api, type ComplianceInstalledPluginPack } from './api'
import type { PluginKind } from './pluginResult'

/** 旧深链 `/compliance`（N12 检查页）的落点：产品唯一内置的判定能力（电商合规） */
export const BUILTIN_JUDGE_PLUGIN_ID = 'com.soujpg.pack.ecommerce'

/** 能力详情页路由约定（P1 定下，P2 详情页三段 / P3 选中集入口都用这一条） */
export function capabilityPath(id: string): string {
  return `/processing/${encodeURIComponent(id)}`
}

/**
 * 能力名 / 一句话描述的**唯一取值口**（导航、能力面板、详情页共用）。
 *
 * 三级回落：接口给的短名 → 接口给的完整名 → id 本身（只在真拿不到名字时出现）。
 * 空串按「没给」处理 —— 声明了但是空的名字不能变成一个空菜单项。
 */
export function capabilityNameOf(p: {
  id: string
  name?: string | null
}): string {
  const given = p.name?.trim()
  return given && given.length > 0 ? given : p.id
}

/** 一句话描述：没声明就是空（不编造），界面据此决定要不要显示描述行 */
export function capabilityBlurbOf(p: { summary?: string | null }): string {
  return p.summary?.trim() ?? ''
}

export interface CapabilityNavItem {
  /** 包 id（账本行 id，testid 与详情路由都由它派生） */
  id: string
  to: string
  /** 能力名（接口下发；真拿不到才退 id） */
  label: string
  /** 一句话「做什么 + 有什么用」（导航 title 提示；未声明 = 空串） */
  blurb: string
  /**
   * manifest kinds（judge=检查 / process=处理；N5b 起随版本行下发）。
   * P3 选择集入口据此区分「检查这些图能不能用」和「把图变成我要的样子」。
   */
  kinds: PluginKind[]
}

interface PacksLike {
  plugins?: ComplianceInstalledPluginPack[]
}

/**
 * 一个能力的 manifest kinds：优先**当前生效版本行**的 `kinds`（N5b 起账本行/目录行都带）；
 * 没有当前版本行时退到任一带有效 kinds 的版本行（内置包首注册即此形态，见下）。
 * 闭集外的值丢弃；一条有效值都没有 → 判定（与后端 `kinds_json` 的缺省一致，不猜成处理）。
 */
function currentKinds(p: ComplianceInstalledPluginPack): PluginKind[] {
  const rows = p.versions ?? []
  const cur = rows.find((v) => v.version === p.currentVersion)
  const fallback = rows.find((v) => (v.kinds ?? []).some((k) => k === 'judge' || k === 'process'))
  const source = cur ?? fallback ?? null
  const ks = (source?.kinds ?? []).filter((k): k is PluginKind => k === 'judge' || k === 'process')
  return ks.length > 0 ? ks : ['judge']
}

/**
 * 已激活能力 → 导航项（纯函数，单测直接钉这里）。
 *
 * 「已激活」口径（P3 修订）：**只看 `runtime.available`** —— 后端读 manifest 实算的静态
 * 装载态（目录在 + manifest 解析得开），缺包/包坏/指针失效时它是 false。
 * P1 原判据 `currentVersion != null` 有个实测漏洞：内置判定包首次被注册时，账本目录指向
 * 仓库包目录而不在 `dam/packs` store 里 → `currentVersion=null`（staleCurrent），但能力
 * 完全可跑 —— 用指针判会把能用的内置能力藏掉（fresh 库上图库「送去处理」面板空）。
 */
export function capabilityNavItems(packs: PacksLike | null | undefined): CapabilityNavItem[] {
  return (packs?.plugins ?? [])
    .filter((p) => p.runtime?.available === true)
    .sort((a, b) => a.id.localeCompare(b.id))
    .map((p) => ({
      id: p.id,
      to: capabilityPath(p.id),
      label: capabilityNameOf(p),
      blurb: capabilityBlurbOf(p),
      kinds: currentKinds(p),
    }))
}

/**
 * 装 / 卸 / 切版本后让导航立即增减：PackManager（安装）与 PluginDetail（切版本 /
 * 回滚 / 卸载）在动作成功后广播；useCapabilityNav 监听并重拉注册表。事件无负载，
 * 纯触发器（真源永远是后端账本，不在前端攒状态）。
 */
export const PACKS_CHANGED_EVENT = 'dam:packs-changed'

export function notifyPacksChanged(): void {
  window.dispatchEvent(new CustomEvent(PACKS_CHANGED_EVENT))
}

/**
 * 导航用的已装能力清单。`loaded` 区分「还没读到」与「读到了 0 个」：前者不渲染
 * 该区（不闪引导文案），后者显示安装引导。
 */
export function useCapabilityNav(): { items: CapabilityNavItem[]; loaded: boolean } {
  const [packs, setPacks] = useState<PacksLike | null>(null)
  useEffect(() => {
    let alive = true
    const load = () =>
      api
        .compliancePacks()
        .then((r) => {
          if (alive) setPacks({ plugins: r.plugins })
        })
        .catch(() => {
          // 读不到注册表 ≠ 没装能力：保持 null（整区不渲染），不误显「去设置安装」
        })
    void load()
    const h = () => void load()
    window.addEventListener(PACKS_CHANGED_EVENT, h)
    return () => {
      alive = false
      window.removeEventListener(PACKS_CHANGED_EVENT, h)
    }
  }, [])
  return { items: capabilityNavItems(packs), loaded: packs != null }
}

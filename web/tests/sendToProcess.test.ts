// @vitest-environment jsdom
/**
 * KI35-P3（task）：送去处理**纯逻辑**单测。
 *
 * 钉死四件事：
 *  1. 能力动词词 / 「全部图库」门禁由 manifest kinds 决定（judge=检查，process=处理，
 *     处理类不给一键改全库的兜底）；
 *  2. capabilityNavItems 派生 kinds（当前生效版本行；缺声明按判定缺省）；
 *  3. 方案分组：官方默认（originKey 非空）/ 我的方案（originKey 空）；停用的不列；
 *  4. 范围选项：当前选中永远有；筛选结果只在寄存了上下文时出现；全部只给纯判定能力。
 */
import { describe, expect, it } from 'vitest'

import type { ComplianceInstalledPluginPack, SpecView } from '../src/lib/api'
import { capabilityNavItems } from '../src/lib/capabilityNav'
import {
  allowAllRange,
  capabilityVerbWord,
  loadProcessFilterScope,
  planGroups,
  rangeOptions,
  specsDerivedCapabilities,
  stashProcessFilterScope,
  usablePlanCount,
} from '../src/lib/sendToProcess'

function pack(id: string, kinds: string[] | null, over: Partial<ComplianceInstalledPluginPack> = {}): ComplianceInstalledPluginPack {
  const versions = kinds
    ? [
        {
          version: '1.0.0',
          dir: `/packs/${id}/1.0.0`,
          present: true,
          manifestHash: null,
          apiVersion: '1',
          deterministic: true,
          signatureStatus: 'unsigned' as const,
          installedAt: 0,
          current: true,
          registered: true,
          kinds,
        },
      ]
    : []
  return { builtin: false, currentVersion: kinds ? '1.0.0' : null, staleCurrent: null, versions, runtime: { available: true }, ...over, id }
}

function spec(specId: number, over: Partial<SpecView> = {}): SpecView {
  return {
    specId,
    pluginId: 'p',
    pluginVersionPin: null,
    name: `方案 ${specId}`,
    industry: null,
    rules: [],
    thresholds: {},
    actions: [],
    output: {},
    paramsDiff: {},
    enabled: 1,
    originKey: null,
    createdAt: 0,
    updatedAt: 0,
    pluginStatus: {
      installed: true,
      pluginName: '合规检查',
      pluginSummary: '检查图片能不能直接用在电商平台（淘宝 / Amazon）上',
      version: '1',
      versionMismatch: false,
      paramsSchemaDeclared: true,
      rulesAvailable: [],
      note: null,
    },
    ...over,
  }
}

describe('动词词与「全部」门禁：由 manifest kinds 决定', () => {
  it('judge → 检查；process → 处理；双占 → 检查 + 处理；缺省 → 检查', () => {
    expect(capabilityVerbWord(['judge'])).toBe('检查')
    expect(capabilityVerbWord(['process'])).toBe('处理')
    expect(capabilityVerbWord(['judge', 'process'])).toBe('检查 + 处理')
    expect(capabilityVerbWord(null)).toBe('检查')
    expect(capabilityVerbWord([])).toBe('检查')
  })

  it('全部图库只给纯判定能力（处理类/双占都不给一键改全库）', () => {
    expect(allowAllRange(['judge'])).toBe(true)
    expect(allowAllRange(['process'])).toBe(false)
    expect(allowAllRange(['judge', 'process'])).toBe(false)
    expect(allowAllRange(null)).toBe(false)
  })
})

describe('capabilityNavItems 派生 kinds（P3：能力面板与导航共用）', () => {
  it('kinds 取当前生效版本行；版本行没有 kinds → 判定缺省（与后端 kinds_json 一致）', () => {
    // 旧账本行形态：版本行在、但没有 kinds 字段
    const legacy: ComplianceInstalledPluginPack = {
      id: 'com.soujpg.pack.realestate',
      builtin: false,
      currentVersion: '1.0.0',
      staleCurrent: null,
      versions: [
        {
          version: '1.0.0',
          dir: '/packs/realestate/1.0.0',
          present: true,
          manifestHash: null,
          apiVersion: '1',
          deterministic: true,
          signatureStatus: 'unsigned',
          installedAt: 0,
          current: true,
          registered: true,
        },
      ],
      runtime: { available: true },
    }
    const items = capabilityNavItems({
      plugins: [pack('com.soujpg.pack.ecommerce', ['judge']), pack('com.soujpg.pack.segment', ['process']), legacy],
    })
    const byId = Object.fromEntries(items.map((i) => [i.id, i]))
    expect(byId['com.soujpg.pack.ecommerce'].kinds).toEqual(['judge'])
    expect(byId['com.soujpg.pack.segment'].kinds).toEqual(['process'])
    expect(byId['com.soujpg.pack.realestate'].kinds).toEqual(['judge'])
  })

  it('kinds 里的闭集外值丢弃，剩不下有效值就按判定（不猜成处理）', () => {
    const items = capabilityNavItems({ plugins: [pack('x', ['weird'] as unknown as string[])] })
    expect(items[0].kinds).toEqual(['judge'])
  })
})

describe('方案分组：官方默认 / 我的方案', () => {
  it('originKey 非空 = 官方默认；空 = 我的方案；停用的不列', () => {
    const g = planGroups([
      spec(1, { originKey: 'seed:ecommerce-amazon', name: '亚马逊主图' }),
      spec(2, { originKey: null, name: '我的 800px' }),
      spec(3, { originKey: null, name: '停用的', enabled: 0 }),
    ])
    expect(g.official.map((s) => s.specId)).toEqual([1])
    expect(g.mine.map((s) => s.specId)).toEqual([2])
    expect(usablePlanCount(g)).toBe(2)
  })
})

describe('范围选项', () => {
  it('当前选中永远有；筛选结果只在寄存上下文时出现；全部只给纯判定能力', () => {
    const base = { selected: 2, filterScope: null, filterTotal: null }
    // 判定类、无筛选上下文：选中 + 全部
    expect(rangeOptions({ ...base, kinds: ['judge'] }).map((r) => r.kind)).toEqual(['selection', 'all'])
    // 处理类：不给全部
    expect(rangeOptions({ ...base, kinds: ['process'] }).map((r) => r.kind)).toEqual(['selection'])
    // 处理类 + 有筛选上下文：选中 + 筛选（仍无全部）
    expect(
      rangeOptions({ ...base, kinds: ['process'], filterScope: { label: '图库当前筛选', filter: {} } }).map((r) => r.kind),
    ).toEqual(['selection', 'filter'])
    // 读数没回来 → count 为 null（界面显示 …，不拿 0 冒充）
    const opts = rangeOptions({
      selected: 2,
      filterScope: { label: '图库当前筛选', filter: {} },
      filterTotal: null,
      kinds: ['judge'],
    })
    expect(opts.find((r) => r.kind === 'filter')?.count).toBeNull()
    expect(opts.find((r) => r.kind === 'selection')?.count).toBe(2)
    // 空选择集 → 文案引导先选图
    expect(rangeOptions({ selected: 0, filterScope: null, filterTotal: null, kinds: ['judge'] })[0].label).toContain(
      '先在图库选图',
    )
  })
})

describe('能力清单补集（fresh 库：注册表还没有内置包的行）', () => {
  it('方案在装（pluginStatus.installed）且注册表没有 → 补出能力行；kinds 由方案派生', () => {
    const extra = specsDerivedCapabilities(
      [
        spec(1, { pluginId: 'com.soujpg.pack.ecommerce', originKey: 'legacy-pack:ecommerce-taobao' }),
        spec(2, { pluginId: 'com.soujpg.pack.ecommerce', originKey: null, actions: [{ action: 'resize' }] }),
        // 停用的不算
        spec(3, { pluginId: 'com.soujpg.pack.upscale', enabled: 0 }),
        // 能力不在装的不算
        spec(4, { pluginId: 'com.soujpg.pack.segment', pluginStatus: { installed: false, version: null, versionMismatch: false, paramsSchemaDeclared: false, rulesAvailable: [], note: '未安装' } }),
      ],
      new Set(['com.soujpg.pack.imaging']),
    )
    expect(extra.map((c) => c.id)).toEqual(['com.soujpg.pack.ecommerce'])
    // DM-B1：名字/描述由接口下发（`pluginStatus.pluginName/pluginSummary`，来自包自己的
    // manifest），前端没有 id → 名字 映射表
    expect(extra[0].label).toBe('合规检查')
    expect(extra[0].blurb).toContain('电商平台')
    // 判定+处理方案并存 → kinds 双占
    expect(extra[0].kinds).toEqual(['judge', 'process'])
    // 注册表已有的 id 不重复补
    expect(specsDerivedCapabilities([spec(9, { pluginId: 'com.soujpg.pack.imaging' })], new Set(['com.soujpg.pack.imaging']))).toEqual([])
  })
})

describe('「当前筛选结果」寄存（sessionStorage，与导出同口径）', () => {
  it('stash → load 同一对象；stash(null) → 清空；脏数据 → null（不猜）', () => {
    expect(loadProcessFilterScope()).toBeNull()
    const scope = { label: '搜索当前条件', filter: { tag: 'cat' } }
    stashProcessFilterScope(scope)
    expect(loadProcessFilterScope()).toEqual(scope)
    stashProcessFilterScope(null)
    expect(loadProcessFilterScope()).toBeNull()
  })
})

/**
 * KI-39（task）：处理类插件的**官方默认方案**在界面上真的出现在「官方默认」分组里。
 *
 * 后端 `spec_seed` 现在为「恰好声明一个动作」的处理类包派生一条 `originKey` 非空的方案
 * （段包 1.2.1：`seed:com.soujpg.pack.segment`，动作链 `[white_bg_fill]`）。这里钉死界面侧
 * 的口径：它属于「官方默认」而不是「我的方案」，且它的动词是**处理**（不是检查）——
 * 否则用户仍然「发不动」（现象 1）。
 */
describe('KI-39：处理类官方默认方案（后端 seed 下发的那一条）', () => {
  const seededSegment = spec(101, {
    pluginId: 'com.soujpg.pack.segment',
    originKey: 'seed:com.soujpg.pack.segment',
    name: '抠图白底',
    actions: [{ action: 'white_bg_fill' }],
    pluginStatus: {
      installed: true,
      pluginName: '抠图换背景',
      pluginSummary: '把图片里的主体抠出来，换成纯色或新背景',
      version: '1.2.1',
      versionMismatch: false,
      paramsSchemaDeclared: true,
      rulesAvailable: [],
      note: null,
    },
  })

  it('出现在「官方默认」分组，且不是「我的方案」', () => {
    const g = planGroups([seededSegment])
    expect(g.official.map((s) => s.specId)).toEqual([101])
    expect(g.mine).toEqual([])
    expect(usablePlanCount(g)).toBe(1)
  })

  it('动词是「处理」（有动作链），并有名字说清做什么', () => {
    expect(capabilityVerbWord(['process'])).toBe('处理')
    expect(seededSegment.name).toBe('抠图白底')
    expect(seededSegment.actions.map((a) => a.action)).toEqual(['white_bg_fill'])
  })

  it('注册表里还没有这一行时（fresh 库），能力面板靠方案补出「处理」能力', () => {
    const extra = specsDerivedCapabilities([seededSegment], new Set())
    expect(extra.map((c) => c.id)).toEqual(['com.soujpg.pack.segment'])
    expect(extra[0].label).toBe('抠图换背景')
    expect(extra[0].kinds).toEqual(['process'])
  })
})

/**
 * N12 入口收敛：规格分类与选择的纯函数口径（`lib/specs.ts`）。
 *
 * 三条卡上写死的规则，逐条钉在这里：
 *  1. 有动作链 = 处理，没有 = 检查（分类只看数据，不看插件目录、不硬编码 id）；
 *  2. 该动词下**只有一条**规格 → 默认选中；多条 → 不替用户选；
 *  3. 停用 / 能力未装 / 版本漂移 → 不能跑，并且给得出人话原因。
 */
import { describe, expect, it } from 'vitest'

import type { SpecView } from '../src/lib/api'
import {
  actionChainText,
  assetSetOptions,
  chainAffectsBody,
  parseIds,
  pickDefaultSpec,
  specRunBlockedReason,
  specSubtitle,
  specVerb,
  specsForVerb,
} from '../src/lib/specs'

function spec(over: Partial<SpecView> & { specId: number }): SpecView {
  return {
    pluginId: 'com.example.detect',
    pluginVersionPin: null,
    name: `规格 ${over.specId}`,
    industry: null,
    rules: ['R_size', 'R_white'],
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
      version: '1.0.0',
      versionMismatch: false,
      paramsSchemaDeclared: true,
      rulesAvailable: ['R_size', 'R_white'],
      note: null,
    },
    ...over,
  }
}

const CHECK_A = spec({ specId: 1 })
const CHECK_B = spec({ specId: 2 })
const PROCESS_A = spec({
  specId: 3,
  pluginId: 'com.example.imaging',
  name: '800px webp',
  actions: [{ action: 'resize', params: { longEdge: 800 } }, { action: 'convert', params: { format: 'webp' } }],
})

describe('动词分类：有动作链 = 处理，没有 = 检查', () => {
  it('空动作链 → check；非空 → process', () => {
    expect(specVerb(CHECK_A)).toBe('check')
    expect(specVerb(PROCESS_A)).toBe('artifact')
  })

  it('按动词筛规格；停用的一律不列（它不能被任务引用）', () => {
    const all = [CHECK_A, CHECK_B, PROCESS_A, spec({ specId: 4, enabled: 0 })]
    expect(specsForVerb(all, 'check').map((s) => s.specId)).toEqual([1, 2])
    expect(specsForVerb(all, 'artifact').map((s) => s.specId)).toEqual([3])
  })
})

describe('默认选中：只有一条就替用户选，多条不猜', () => {
  it('单条 → 自动选中（用户不用再点）', () => {
    expect(pickDefaultSpec([PROCESS_A], 'artifact', null)?.specId).toBe(3)
  })

  it('多条 → null（必须显式选，否则「这次用了哪条」说不清）', () => {
    expect(pickDefaultSpec([CHECK_A, CHECK_B], 'check', null)).toBeNull()
  })

  it('沿用上次选择（深链/回来时），但跨动词不串味', () => {
    expect(pickDefaultSpec([CHECK_A, CHECK_B], 'check', 2)?.specId).toBe(2)
    // 2 是检查类规格，处理页不认它 → 回落到「处理只有一条」的默认
    expect(pickDefaultSpec([CHECK_B, PROCESS_A], 'artifact', 2)?.specId).toBe(3)
  })
})

describe('能不能跑：环境事实来自 pluginStatus，不猜', () => {
  it('可用 → 不阻断', () => {
    expect(specRunBlockedReason(CHECK_A)).toBeNull()
  })

  it('没有方案 → 提示先选', () => {
    expect(specRunBlockedReason(null)).toContain('先选一条方案')
  })

  it('停用 → 说停用', () => {
    expect(specRunBlockedReason(spec({ specId: 9, enabled: 0 }))).toContain('已停用')
  })

  it('能力未装 → 用人话（优先用服务端 note）', () => {
    const un = spec({
      specId: 10,
      pluginStatus: {
        installed: false,
        version: null,
        versionMismatch: false,
        paramsSchemaDeclared: false,
        rulesAvailable: [],
        note: '能力未安装：规格保留可见，装好后再执行',
      },
    })
    expect(specRunBlockedReason(un)).toContain('装好后再执行')
  })

  it('版本漂移 → 说清「不一致」，不静默按新版本跑', () => {
    const drift = spec({
      specId: 11,
      pluginStatus: {
        installed: true,
        version: '2.0.0',
        versionMismatch: true,
        paramsSchemaDeclared: true,
        rulesAvailable: [],
        note: null,
      },
    })
    expect(specRunBlockedReason(drift)).toContain('2.0.0')
  })
})

describe('副标题与动作链文案', () => {
  it('检查类显示规则数（0 条 = 全部规则）', () => {
    expect(specSubtitle(CHECK_A)).toBe('2 条规则')
    expect(specSubtitle(spec({ specId: 20, rules: [] }))).toBe('全部规则')
  })

  it('处理类显示动作链，顺序即执行顺序', () => {
    expect(actionChainText(PROCESS_A)).toBe('resize → convert')
    expect(specSubtitle(PROCESS_A)).toBe('resize → convert')
  })

  it('危险动作（改本体）要能识别出来（界面据此二次确认）', () => {
    expect(chainAffectsBody(PROCESS_A)).toBe(true)
    expect(chainAffectsBody(CHECK_A)).toBe(false)
  })
})

describe('图集取值', () => {
  it('处理的图集只有「选中集」——不给「全部」兜底（防一键改全库）', () => {
    const opts = assetSetOptions('artifact', 3)
    expect(opts.map((o) => o.kind)).toEqual(['selection'])
    expect(opts[0].label).toContain('3 张')
  })

  it('检查可以有「还没检查过的」「全部图库」', () => {
    const opts = assetSetOptions('check', 0, 12, 100)
    expect(opts.map((o) => o.kind)).toEqual(['selection', 'pending', 'all'])
    expect(opts[1].count).toBe(12)
    expect(opts[2].count).toBe(100)
  })

  it('没选图时也说清是 0 张（不许留空白让人猜）', () => {
    expect(assetSetOptions('artifact', 0)[0].label).toContain('0 张')
  })
})

describe('深链 ids 解析', () => {
  it('1,2,3 → [1,2,3]；坏值/重复/非正数丢掉', () => {
    expect(parseIds('1,2,3')).toEqual([1, 2, 3])
    expect(parseIds('1,abc,1,-2,0,3')).toEqual([1, 3])
    expect(parseIds(null)).toEqual([])
    expect(parseIds('')).toEqual([])
  })
})

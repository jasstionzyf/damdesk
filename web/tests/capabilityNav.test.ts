// @vitest-environment jsdom
/**
 * KI-35 P1（t_0b283e20）+ DM-B1（t_7a23bc3b）：主导航能力区 = 已装包注册表驱动的**纯函数**口径。
 *
 * 验收点：
 *  1. 已激活（装载可用）→ 出导航项，显示**接口下发的能力名**（不是 id）；
 *  2. 缺包 / 包坏 / 指针失效 → 不出（装好了才出现）；
 *  3. DM-B1：能力名来自 `GET /api/compliance/packs` 的 `plugins[].name`（manifest
 *     `short_name` → `name`）—— 装了映射外的能力（如第三方副本）也显示中文名，
 *     前端**没有** id → 名字 的映射表；接口真没给才退 id；
 *  4. 顺序稳定（按 id 排）。
 */
import { describe, expect, it } from 'vitest'

import type { ComplianceInstalledPluginPack } from '../src/lib/api'
import {
  BUILTIN_JUDGE_PLUGIN_ID,
  capabilityBlurbOf,
  capabilityNameOf,
  capabilityNavItems,
  capabilityPath,
} from '../src/lib/capabilityNav'

function pack(over: Partial<ComplianceInstalledPluginPack> & { id: string }): ComplianceInstalledPluginPack {
  return {
    builtin: false,
    currentVersion: '1.0.0',
    staleCurrent: null,
    versions: [],
    runtime: { available: true },
    ...over,
  }
}

/** 接口真实形态（reports/evidence/t_7a23bc3b/packs.json）：名字/描述都随包下发 */
const EC = {
  id: 'com.soujpg.pack.ecommerce',
  name: '合规检查',
  summary: '检查图片能不能直接用在电商平台（淘宝 / Amazon）上',
}
const IMG = { id: 'com.soujpg.pack.imaging', name: '图片处理', summary: '批量缩放、裁剪、转格式、压缩、加水印、改名' }
const SEG = { id: 'com.soujpg.pack.segment', name: '抠图换背景', summary: '把图片里的主体抠出来，换成纯色或新背景' }

describe('capabilityNavItems：已装能力 → 导航项', () => {
  it('0 个包 → 0 项（导航该区收起，交给空态引导渲染层决定）', () => {
    expect(capabilityNavItems(null)).toEqual([])
    expect(capabilityNavItems({})).toEqual([])
    expect(capabilityNavItems({ plugins: [] })).toEqual([])
  })

  it('1 个已激活包 → 1 项，名字/描述都来自接口，路由是 /processing/<id>', () => {
    const items = capabilityNavItems({ plugins: [pack({ ...EC, builtin: true })] })
    expect(items).toHaveLength(1)
    expect(items[0].label).toBe('合规检查')
    expect(items[0].blurb).toBe('检查图片能不能直接用在电商平台（淘宝 / Amazon）上')
    expect(items[0].to).toBe('/processing/com.soujpg.pack.ecommerce')
  })

  it('N 个已激活包 → 逐项出现，顺序按 id 稳定排（不随后端返回顺序跳）', () => {
    const items = capabilityNavItems({
      plugins: [pack(SEG), pack({ ...EC, builtin: true }), pack({ ...IMG, builtin: true })],
    })
    expect(items.map((i) => i.label)).toEqual(['合规检查', '图片处理', '抠图换背景'])
    expect(items.map((i) => i.id)).toEqual([
      'com.soujpg.pack.ecommerce',
      'com.soujpg.pack.imaging',
      'com.soujpg.pack.segment',
    ])
  })

  it('卸载/包坏 → 不出导航（真实形态：目录没了 → runtime.available=false，后端实算）', () => {
    const items = capabilityNavItems({
      plugins: [
        pack({
          id: 'com.soujpg.pack.segment',
          currentVersion: null,
          staleCurrent: '1.2.0',
          runtime: { available: false, reason: 'plugin_pack_missing' },
        }),
        pack({
          id: 'com.soujpg.pack.upscale',
          runtime: { available: false, reason: 'plugin_manifest_invalid', note: 'bad json' },
        }),
        pack({ ...EC, builtin: true }),
      ],
    })
    expect(items.map((i) => i.id)).toEqual(['com.soujpg.pack.ecommerce'])
  })

  it('P3 修订：内置包首注册形态（currentVersion=null，账本目录指仓库包）但装载可用 → 照样出', () => {
    // 实测（reports/evidence/t_06bb84bd/03-packs.json）：内置判定包第一次被跑判定注册进账本时，
    // 目录不在 dam/packs store → currentVersion=null/staleCurrent=1.3.0，但 runtime.available=true。
    const items = capabilityNavItems({
      plugins: [
        pack({
          ...EC,
          builtin: true,
          currentVersion: null,
          staleCurrent: '1.3.0',
          versions: [
            {
              version: '1.3.0',
              dir: 'repo/packs/plugins/ecommerce',
              present: true,
              manifestHash: null,
              apiVersion: '1.1',
              deterministic: false,
              signatureStatus: 'unsigned',
              installedAt: 0,
              current: true,
              registered: true,
              outputs: ['verdict'],
            },
          ],
        }),
      ],
    })
    expect(items).toHaveLength(1)
    expect(items[0].id).toBe('com.soujpg.pack.ecommerce')
    // outputs 从版本行退读（当前版本行不存在，退到任一带有效 outputs 的行）
    expect(items[0].outputs).toEqual(['verdict'])
  })

  it('DM-B1：映射外的能力（第三方副本）显示接口给的中文名，不显示裸 id', () => {
    // KI-36 现象：`com.soujpg.pack.imaging_demo` 在导航里是裸 id（前端映射表只覆 5 个官方 id）。
    // 现在名字随包下发：副本沿用复制源的 manifest 名字，导航直接显示中文名。
    const items = capabilityNavItems({
      plugins: [pack({ id: 'com.soujpg.pack.imaging_demo', name: '图片处理', summary: '批量缩放、裁剪、转格式、压缩、加水印、改名' })],
    })
    expect(items[0].label).toBe('图片处理')
    expect(items[0].blurb).toContain('批量缩放')
    expect(items[0].label).not.toBe('com.soujpg.pack.imaging_demo')
  })

  it('接口真没给名字（包目录不在 / manifest 不可解析）→ 退显 id，缺口一眼可见', () => {
    const items = capabilityNavItems({ plugins: [pack({ id: 'com.example.custom' })] })
    expect(items[0].label).toBe('com.example.custom')
    expect(items[0].blurb).toBe('')
  })

  it('名字声明了但是空串/纯空白 → 按没给处理（退 id，不出空菜单项）', () => {
    const items = capabilityNavItems({ plugins: [pack({ id: 'com.example.blank', name: '   ', summary: '  ' })] })
    expect(items[0].label).toBe('com.example.blank')
    expect(items[0].blurb).toBe('')
  })
})

describe('名字/描述取值口（导航、面板、详情共用一份）', () => {
  it('capabilityNameOf：接口名 → id；capabilityBlurbOf：未声明 = 空串（不编造）', () => {
    expect(capabilityNameOf({ id: 'a.b', name: '合规检查' })).toBe('合规检查')
    expect(capabilityNameOf({ id: 'a.b', name: null })).toBe('a.b')
    expect(capabilityNameOf({ id: 'a.b' })).toBe('a.b')
    expect(capabilityBlurbOf({ summary: '做什么' })).toBe('做什么')
    expect(capabilityBlurbOf({})).toBe('')
  })
})

describe('路由约定（P1 定，P2/P3 对齐用）', () => {
  it('capabilityPath：/processing/:pluginId，id 做 encodeURIComponent', () => {
    expect(capabilityPath('com.soujpg.pack.imaging')).toBe('/processing/com.soujpg.pack.imaging')
    expect(capabilityPath('a b/c')).toBe('/processing/a%20b%2Fc')
  })

  it('旧 /compliance 深链落点 = 内置判定能力（电商合规）', () => {
    expect(BUILTIN_JUDGE_PLUGIN_ID).toBe('com.soujpg.pack.ecommerce')
  })
})

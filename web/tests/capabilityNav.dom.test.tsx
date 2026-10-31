// @vitest-environment jsdom
/**
 * KI-35 P1（task）：主导航能力区的**渲染**与**装/卸即时增减**（mock /api/compliance/packs）。
 *
 * 验收点（卡 task）：
 *  1. 0/1/N 个已装能力 → 导航渲染对应数量，显示能力名；
 *  2. 一个都没装 → 安装引导（指向设置 › 装包 Drawer），不是灰按钮；
 *  3. 注册表还没读到（loaded=false）→ 该区不渲染（不闪「去设置安装」的空态）；
 *  4. 装/卸成功（dam:packs-changed 事件）→ 导航**立即**增减，不用刷新页面。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act, cleanup, render, renderHook, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

import { CapabilityNavSection } from '../src/App'
import type { ComplianceInstalledPluginPack } from '../src/lib/api'
import { PACKS_CHANGED_EVENT, capabilityNavItems, useCapabilityNav } from '../src/lib/capabilityNav'

const apiStub = vi.hoisted(() => ({ compliancePacks: vi.fn() }))
vi.mock('../src/lib/api', () => ({ api: apiStub }))

const EC = 'com.soujpg.pack.ecommerce'
const IMG = 'com.soujpg.pack.imaging'
const SEG = 'com.soujpg.pack.segment'

/** 接口真实形态（DM-B1）：名字/描述随包下发，前端不维护映射表 */
const NAMES: Record<string, { name: string; summary: string }> = {
  [EC]: { name: '合规检查', summary: '检查图片能不能直接用在电商平台（淘宝 / Amazon）上' },
  [IMG]: { name: '图片处理', summary: '批量缩放、裁剪、转格式、压缩、加水印、改名' },
  [SEG]: { name: '抠图换背景', summary: '把图片里的主体抠出来，换成纯色或新背景' },
}

function pack(id: string, over: Partial<ComplianceInstalledPluginPack> = {}): ComplianceInstalledPluginPack {
  return {
    builtin: false,
    currentVersion: '1.0.0',
    staleCurrent: null,
    versions: [],
    runtime: { available: true },
    ...NAMES[id],
    ...over,
    id,
  }
}

function renderSection(items: ReturnType<typeof capabilityNavItems>, loaded: boolean) {
  return render(
    <MemoryRouter>
      <CapabilityNavSection items={items} loaded={loaded} />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  apiStub.compliancePacks.mockReset()
})
afterEach(() => cleanup())

describe('CapabilityNavSection 渲染：0 / 1 / N', () => {
  it('N 个已装能力 → N 个导航项，显示能力名 + 一句话描述（title）', () => {
    const items = capabilityNavItems({
      plugins: [pack('com.soujpg.pack.ecommerce', { builtin: true }), pack('com.soujpg.pack.segment')],
    })
    renderSection(items, true)
    const a = screen.getByTestId('nav-capability-com.soujpg.pack.ecommerce')
    const b = screen.getByTestId('nav-capability-com.soujpg.pack.segment')
    expect(a.textContent).toContain('合规检查')
    expect(b.textContent).toContain('抠图换背景')
    expect(a.getAttribute('title')).toContain('电商平台')
    expect(b.getAttribute('href')).toBe('/processing/com.soujpg.pack.segment')
  })

  it('DM-B1：映射外的能力（第三方副本）按接口下发的名字渲染，不显示裸 id', () => {
    // KI-36 现象复现口径：`com.soujpg.pack.imaging_demo` 是仓库里没有的 id（前端没有映射）。
    const items = capabilityNavItems({
      plugins: [
        pack('com.soujpg.pack.imaging_demo', { name: '图片处理', summary: '批量缩放、裁剪、转格式、压缩、加水印、改名' }),
      ],
    })
    renderSection(items, true)
    const a = screen.getByTestId('nav-capability-com.soujpg.pack.imaging_demo')
    expect(a.textContent).toContain('图片处理')
    expect(a.textContent).not.toContain('com.soujpg')
    expect(a.getAttribute('title')).toContain('批量缩放')
  })

  it('0 个已装能力 → 安装引导（不是灰按钮），指向设置 › 装包 Drawer', () => {
    renderSection([], true)
    const guide = screen.getByTestId('nav-capabilities-empty')
    expect(guide.textContent).toContain('还没有可用的处理功能')
    const link = screen.getByRole('link', { name: '设置' })
    expect(link.getAttribute('href')).toBe('/settings?drawer=packs')
  })

  it('注册表还没读到（loaded=false）→ 该区整体不渲染（不闪空态引导）', () => {
    const { container } = renderSection([], false)
    expect(container.querySelector('[data-testid="nav-capabilities-empty"]')).toBeNull()
    expect(container.querySelectorAll('a')).toHaveLength(0)
  })
})

describe('useCapabilityNav：装/卸后导航立即增减（mock /api/compliance/packs）', () => {
  it('首拉渲染 → 装 imaging 包（事件触发重拉）→ 导航多一项 → 卸掉 → 回到引导态', async () => {
    apiStub.compliancePacks.mockResolvedValue({ plugins: [pack('com.soujpg.pack.ecommerce', { builtin: true })] })
    const { result } = renderHook(() => useCapabilityNav())

    await waitFor(() => expect(result.current.loaded).toBe(true))
    expect(result.current.items.map((i) => i.label)).toEqual(['合规检查'])

    // 安装 imaging 包成功（PackManager 广播 dam:packs-changed）→ 导航立即多一项
    apiStub.compliancePacks.mockResolvedValue({
      plugins: [pack('com.soujpg.pack.ecommerce', { builtin: true }), pack('com.soujpg.pack.imaging', { builtin: true })],
    })
    act(() => {
      window.dispatchEvent(new CustomEvent(PACKS_CHANGED_EVENT))
    })
    await waitFor(() => expect(result.current.items.map((i) => i.label)).toEqual(['合规检查', '图片处理']))

    // 卸载 ecommerce（目录删掉 → 后端实算 available=false）→ 导航立即只剩一项
    // （P3 修订：判「已激活」只看 runtime.available；指针/目录形态不再参与判断）
    apiStub.compliancePacks.mockResolvedValue({
      plugins: [
        pack('com.soujpg.pack.ecommerce', {
          builtin: true,
          currentVersion: null,
          staleCurrent: '1.0.0',
          runtime: { available: false, reason: 'plugin_pack_missing' },
        }),
        pack('com.soujpg.pack.imaging', { builtin: true }),
      ],
    })
    act(() => {
      window.dispatchEvent(new CustomEvent(PACKS_CHANGED_EVENT))
    })
    await waitFor(() => expect(result.current.items.map((i) => i.label)).toEqual(['图片处理']))
  })

  it('注册表读不到 ≠ 没装能力：loaded 停在 false（不误显「去设置安装」）', async () => {
    apiStub.compliancePacks.mockRejectedValue(new Error('backend down'))
    const { result } = renderHook(() => useCapabilityNav())
    await waitFor(() => expect(apiStub.compliancePacks).toHaveBeenCalled())
    expect(result.current.loaded).toBe(false)
    expect(result.current.items).toEqual([])
  })
})

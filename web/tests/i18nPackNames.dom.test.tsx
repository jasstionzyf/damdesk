// @vitest-environment jsdom
/**
 * i18n-F（卡 task）：英文界面下**包管理列表的包名**由包自带多语言字段驱动
 * （DOM 级断言 —— 不是「接口返回对」就算过）。
 *
 * 数据形状与后端真实下发一致：`name` 是按 Accept-Language 预取的中文串（本用例模拟
 * 「语言已切 en」时前端用 `nameI18n` 直接取值，不依赖重拉接口）。
 * 判据：DOM innerText 出现英文短名、**不出现中文包名**。
 */
import { act, cleanup, render, renderHook, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'

const packs = vi.hoisted(() => ({
  plugins: [
    {
      id: 'com.soujpg.pack.ecommerce',
      name: '合规检查', // 后端按 zh 预取（模拟「切语言前最后一次响应」）
      nameI18n: { 'zh-Hans': '合规检查', 'en-US': 'Compliance Check' },
      summary: '检查图片能不能直接用在电商平台',
      summaryI18n: {
        'zh-Hans': '检查图片能不能直接用在电商平台',
        'en-US': 'Checks whether product images can be used on e-commerce platforms',
      },
      builtin: true,
      currentVersion: '1.3.0',
      staleCurrent: null,
      versions: [],
      runtime: { available: true, reason: null, note: null, version: '1.3.0' },
    },
  ],
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const mod = await importOriginal<typeof import('../src/lib/api')>()
  return {
    ...mod,
    api: {
      ...mod.api,
      compliancePacks: vi.fn(async () => ({ plugins: packs.plugins })),
      compliancePluginPackDetail: vi.fn(async () => null),
      complianceStatus: vi.fn(async () => ({ pluginState: null, states: {}, queue: {}, feedback: {} })),
    },
  }
})

import { MemoryRouter } from 'react-router-dom'

import { PackManager } from '../src/components/PackManager'
import { useI18n } from '../src/i18n'
import { ToastProvider } from '../src/lib/toast'

// i18n-G 合流：主干的 vitest 全局 setup 把测试默认 locale 钉成 zh-Hans（存量中文断言口径），
// 本用例验的是「en 界面」，因此显式切 en 并在结束后切回，不依赖环境默认值。
afterEach(() => {
  cleanup()
  const { result } = renderHook(() => useI18n())
  act(() => result.current.setLocale('zh-Hans'))
})

describe('包管理列表的包名（i18n-F）', () => {
  it('en 界面：包名显示英文短名，不出现中文包名', async () => {
    const { result: i18n } = renderHook(() => useI18n())
    act(() => i18n.current.setLocale('en-US'))
    render(
      <MemoryRouter>
        <ToastProvider>
          <PackManager />
        </ToastProvider>
      </MemoryRouter>,
    )
    // 列表异步拉取后渲染
    await waitFor(() => {
      expect(screen.getByTestId('plugin-name-com.soujpg.pack.ecommerce')).toBeTruthy()
    })
    const cell = screen.getByTestId('plugin-name-com.soujpg.pack.ecommerce')
    expect(cell.textContent).toBe('Compliance Check')
    // jsdom 没有 innerText，用 textContent 等价扫描整页文本
    const text = document.body.textContent ?? ''
    expect(text).toContain('Compliance Check')
    expect(text).not.toContain('合规检查')
  })
})

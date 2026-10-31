// @vitest-environment jsdom
/**
 * DECouple-FE（卡 task）：复核队列 Dialog 的前端口径。
 *
 * 原 `compliancePackSource.dom.test.tsx` / `complianceSingleSurface.dom.test.tsx` 验的是已下线的
 * `/review` 页与 Inspector 合规块；能力收进任务中心工具栏的低频 Dialog 后，保留下来的**硬口径**是：
 *  1. 机器结论与置信度**原样显示**（改判不改写机器结论）；
 *  2. 插件失败词 `plugin_*` 翻成人话并标「来源 检查能力」（非插件原因不标来源）；
 *  3. 已改判的行同时显示生效结论与改判依据；
 *  4. 没有任何待处理条目时如实说「没有」，不空白。
 *
 * i18n-C：本文件断言的文案走 t()，jsdom 的 navigator.language 默认 en-US，这里把语言钉成
 * zh-Hans（缺译回退 key = 中文原文），断言逐字不变。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

vi.hoisted(() => {
  try {
    if (typeof localStorage !== 'undefined' && localStorage) {
      localStorage.setItem('dam.locale', 'zh-Hans')
      return
    }
  } catch {
    /* 落桩 */
  }
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => (k === 'dam.locale' ? 'zh-Hans' : null),
    setItem: () => {},
    removeItem: () => {},
    clear: () => {},
    key: () => null,
    length: 0,
  })
})

const apiStub = vi.hoisted(() => ({
  thumbUrl: (id: number, size: 256 | 1024 = 256) => `/api/media/thumb/${id}?size=${size}`,
  complianceReview: vi.fn(),
  complianceReviewCreate: vi.fn(),
  complianceReviewRevoke: vi.fn(),
}))

vi.mock('../src/lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../src/lib/api')>()
  return { ...actual, api: { ...actual.api, ...apiStub } }
})

import { ToastProvider } from '../src/lib/toast'
import { ReviewQueueDialog } from '../src/components/ReviewQueueDialog'
import type { ComplianceReviewItem } from '../src/lib/api'

function row(over: Partial<ComplianceReviewItem> = {}): ComplianceReviewItem {
  return {
    assetId: 601,
    fileName: 'shoe.jpg',
    absPath: '/data/lib/shoe.jpg',
    thumbPath: null,
    ruleId: 'logo_missing',
    ruleName: '主图白底',
    judgementScope: 'asset',
    machineResult: 'undetermined',
    machineConfidence: null,
    machineScore: null,
    reason: 'plugin_timeout',
    lowConfidence: true,
    finalStatus: 'undetermined',
    humanReview: null,
    reviewed: false,
    ts: 1700000000,
    ...over,
  }
}

function renderDialog(onChanged = vi.fn()) {
  return render(
    <MemoryRouter>
      <ToastProvider>
        <ReviewQueueDialog open onClose={() => {}} onChanged={onChanged} />
      </ToastProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  apiStub.complianceReview.mockImplementation(async () => ({
    packId: 'ecommerce',
    total: 2,
    threshold: 0.7,
    lowConfidenceOnly: true,
    items: [
      row(),
      // 已改判行：机器结论仍是 undetermined，生效结论 = fail
      row({
        assetId: 602,
        fileName: 'bag.jpg',
        ruleId: null,
        ruleName: null,
        machineResult: 'undetermined',
        finalStatus: 'fail',
        reviewed: true,
        humanReview: {
          id: 1,
          ruleId: null,
          scope: 'asset',
          humanStatus: 'fail',
          reason: '确认背景发灰',
          at: 1700000100,
          reviewedBy: null,
          machineResult: 'undetermined',
          machineConfidence: null,
        },
      }),
    ],
  }))
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('复核队列 Dialog：三档口径', () => {
  it('T1 行内显示机器结论 + 置信度 + 插件原因人话（括号保留原词）', async () => {
    renderDialog()
    const reason = await screen.findByTestId('review-queue-reason-601-logo_missing')
    expect(reason.textContent).toContain('检查超时')
    expect(reason.textContent).toContain('plugin_timeout')
    expect(screen.getByTestId('review-queue-machine-601').textContent).toContain('未判定')
    expect(screen.getByTestId('review-queue-total').textContent).toContain('2')
  })

  it('T2 插件通道才有「来源 检查能力」标记（非插件原因不标）', async () => {
    apiStub.complianceReview.mockImplementation(async () => ({
      packId: 'ecommerce',
      total: 1,
      threshold: 0.7,
      lowConfidenceOnly: true,
      items: [row({ reason: 'rule_error' })],
    }))
    renderDialog()
    const reason = await screen.findByTestId('review-queue-reason-601-logo_missing')
    expect(reason.textContent).toContain('规则执行报错')
    expect(screen.queryByTestId('review-queue-source-plugin-601-logo_missing')).toBeNull()
  })

  it('T3 已改判行同时显示生效结论与改判依据（机器结论不被改写）', async () => {
    renderDialog()
    await screen.findByTestId('review-queue-row-602-asset')
    const final = screen.getByTestId('review-queue-final-602')
    expect(final.textContent).toContain('不合格')
    expect(screen.getByTestId('review-queue-row-602-asset').textContent).toContain('确认背景发灰')
  })

  it('T4 没有待处理条目时如实说「没有」（不空白也不假装在加载）', async () => {
    apiStub.complianceReview.mockImplementation(async () => ({
      packId: 'ecommerce',
      total: 0,
      threshold: 0.7,
      lowConfidenceOnly: true,
      items: [],
    }))
    renderDialog()
    const empty = await screen.findByTestId('review-queue-empty')
    expect(empty.textContent).toContain('没有待人工看')
  })

  it('T5 读取失败时显示原因，不留白屏', async () => {
    apiStub.complianceReview.mockImplementation(async () => {
      throw new Error('boom')
    })
    renderDialog()
    const err = await screen.findByTestId('review-queue-error')
    expect(err.textContent).toContain('boom')
    await waitFor(() => expect(screen.queryByTestId('review-queue-loading')).toBeNull())
  })
})

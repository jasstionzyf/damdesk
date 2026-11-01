// @vitest-environment jsdom
/**
 * DECouple-F3（卡 t_53396ef8）：改判成功回执不被列表刷新抹掉。
 *
 * 真机取证（reports/evidence/DECouple-真机/out/zh_c2/raw-review-network.log：
 * POST 400→force→200，human_review 落行，UI 零反馈）：`JobImageListDialog` 的清理
 * effect 挂在 `[open, items]` 上 —— 改判 200 后 onChanged→load 换 items 引用，
 * `msg` 被一并清空，「已改判为…」永远看不到。
 *
 * 口径（用户裁决）：重载只清输入（reviewStatus/reason），**回执与 items 引用解耦**
 * —— 回执只随 Dialog 关闭（open 离开）或下一次提交消失；改判中的图保持选中，
 * 让回执与刷新后的结论同屏可见。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import type { ReactElement } from 'react'

// i18n-C 同款钉法：jsdom navigator.language 默认 en-US，这里钉 zh-Hans，
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变。
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

import { JobImageListDialog, type JobImageItem } from '../src/components/JobImageListDialog'

function item(over: Partial<JobImageItem> = {}): JobImageItem {
  return {
    assetId: 12,
    fileName: 'a.jpg',
    verdict: 'undetermined',
    reason: '置信度不足',
    thumbUrl: '/t/12',
    fullUrl: '/f/12',
    ...over,
  }
}

function ui(props: { items: JobImageItem[]; open?: boolean }): ReactElement {
  return (
    <JobImageListDialog
      open={props.open ?? true}
      title="这批图"
      items={props.items}
      onReview={vi.fn().mockResolvedValue(undefined)}
      onClose={() => {}}
    />
  )
}

/** 走完一次成功改判：提交 → onReview 内部（复刻 JobResults.review 的 await load）换 items 引用 → resolve */
async function reviewThroughReload(rerender: (u: ReactElement) => void) {
  fireEvent.click(await screen.findByTestId('job-image-cell-12'))
  fireEvent.click(screen.getByTestId('job-image-review-pass'))
  fireEvent.change(screen.getByTestId('job-image-review-reason'), { target: { value: '人工确认可用' } })
  fireEvent.click(screen.getByTestId('job-image-review-submit'))
  void rerender
}

describe('JobImageListDialog 改判回执（DECouple-F3）', () => {
  beforeEach(() => {
    localStorage.setItem('dam.locale', 'zh-Hans')
  })
  afterEach(() => {
    cleanup()
    vi.restoreAllMocks()
  })

  it('改判 200 → 列表重载（items 换引用）→ 回执「已改判为…」仍在、输入已清、明细不弹回网格', async () => {
    let doRender!: (u: ReactElement) => void
    const onReview = vi.fn(async () => {
      // 复刻 JobResults.review：await load(runId) 先换 items 引用，onReview 才 resolve
      doRender(ui({ items: [item({ verdict: 'pass', reason: '人工复核通过' })] }))
    })
    const { rerender } = render(
      <JobImageListDialog open title="这批图" items={[item()]} onReview={onReview} onClose={() => {}} />,
    )
    doRender = rerender
    await reviewThroughReload(rerender)

    const receipt = await screen.findByTestId('job-image-review-result')
    expect(receipt.textContent).toContain('已改判为')
    expect(receipt.textContent).toContain('通过')
    // 重载只清输入
    expect((screen.getByTestId('job-image-review-reason') as HTMLInputElement).value).toBe('')
    expect(screen.getByTestId('job-image-review-pass').getAttribute('aria-pressed')).not.toBe('true')
    // 明细保持选中：回执与刷新后的结论同屏
    expect(screen.getByTestId('job-image-detail')).toBeTruthy()
    expect(screen.getByTestId('job-image-detail-verdict').textContent).toContain('人工复核通过')
  })

  it('回执在后续无关重载下不丢（直到用户关闭/再次提交）', async () => {
    let doRender!: (u: ReactElement) => void
    const onReview = vi.fn(async () => {
      doRender(ui({ items: [item({ verdict: 'pass', reason: '人工复核通过' })] }))
    })
    const { rerender } = render(
      <JobImageListDialog open title="这批图" items={[item()]} onReview={onReview} onClose={() => {}} />,
    )
    doRender = rerender
    await reviewThroughReload(rerender)
    await screen.findByTestId('job-image-review-result')

    // 又一次无关重载（如轮询/其他行刷新）
    rerender(ui({ items: [item({ verdict: 'pass', reason: '人工复核通过', fileName: 'a2.jpg' })] }))
    await waitFor(() => {
      expect(screen.queryByTestId('job-image-review-result')).toBeTruthy()
    })
  })

  it('回执随 Dialog 关闭消失：重开回到干净态（不带旧回执）', async () => {
    let doRender!: (u: ReactElement) => void
    const onReview = vi.fn(async () => {
      doRender(ui({ items: [item({ verdict: 'pass', reason: '人工复核通过' })] }))
    })
    const { rerender } = render(
      <JobImageListDialog open title="这批图" items={[item()]} onReview={onReview} onClose={() => {}} />,
    )
    doRender = rerender
    await reviewThroughReload(rerender)
    await screen.findByTestId('job-image-review-result')

    rerender(ui({ items: [item({ verdict: 'pass', reason: '人工复核通过' })], open: false }))
    rerender(ui({ items: [item({ verdict: 'pass', reason: '人工复核通过' })], open: true }))
    expect(screen.queryByTestId('job-image-review-result')).toBeNull()
    expect(screen.queryByTestId('job-image-detail')).toBeNull()
  })
})

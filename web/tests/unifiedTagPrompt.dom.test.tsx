// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/tag-tasks?open=new-task"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { BrowserRouter } from 'react-router-dom'
import { TAG_PROMPT_FORMAT_CLAUSE } from '../src/lib/tagFormat'

// i18n-C（卡 task）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
// jsdom 的 navigator.language 默认 en-US（会渲染英文）。这里把语言钉成 zh-Hans ——
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变、强度不降，只是显式声明
// 「本文件验中文文案」。vi.hoisted 早于所有 import 执行，故早于 i18n 模块初始化。
vi.hoisted(() => {
  // jsdom 文件有真实 localStorage；node 环境的纯 SSR 渲染测试没有它 ——
  // 给一个最小桩，目的只有一个：让 i18n 初始化时 resolveInitialLocale() 读到 zh-Hans。
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

/**
 * 统一标签裁决（2026-10-19）· 前端引导与强校验（卡 task）：
 *  - 新建自定义打标任务时 prompt 预填输出格式段（离散标签数组 + `_` 连接）；
 *  - 「插入格式要求」按钮一键补齐；
 *  - 保存前校验：prompt 未同时含「数组」与（`_` 或「下划线」）→ 弹确认「未声明输出格式，
 *    标签将无法自解释」，默认阻止保存（window.confirm 取消 = 阻止），确认后才放行。
 */

const apiStub = vi.hoisted(() => ({
  tagStatus: vi.fn(),
  tagTasks: vi.fn(),
  tagFailed: vi.fn(),
  tagCustomFailed: vi.fn(),
  ocrStatus: vi.fn(),
  tagTaskCreate: vi.fn(),
  tagTaskUpdate: vi.fn(),
}))

vi.mock('../src/lib/api', () => ({ api: apiStub }))

vi.mock('../src/lib/tasks', () => ({
  useTasks: () => ({
    tasks: [],
    runningCount: 0,
    startImport: vi.fn(),
    startEmbed: vi.fn(),
    startTag: vi.fn(),
    cancel: vi.fn(),
    retry: vi.fn(),
    retryFailed: vi.fn(),
    dismiss: vi.fn(),
  }),
}))

vi.mock('../src/lib/toast', () => ({
  useToast: () => ({ toast: vi.fn() }),
}))

function stubStatus() {
  const queue = { running: false, progress: null, report: null, lastError: null }
  apiStub.tagStatus.mockImplementation(async () => ({
    template: queue,
    custom: { ...queue, pendingJobs: 0 },
    review: { assets: 0, rows: 0 },
    stats: { untaggedAssets: 0, pending: 0, done: 0, failed: 0 },
    provider: {
      enabled: true,
      configured: true,
      hasKey: true,
      provider: 'stub',
      baseUrl: 'http://stub',
      model: 'stub-model',
      activeModel: 'stub-model',
      maxTokens: 2048,
      maxTokensSource: 'default',
      maxTokensLabel: '',
      defaultMaxTokens: 2048,
    },
    models: [],
  }))
  apiStub.tagTasks.mockImplementation(async () => ({ items: [] }))
  apiStub.tagFailed.mockImplementation(async () => ({ items: [] }))
  apiStub.tagCustomFailed.mockImplementation(async () => ({ items: [] }))
  apiStub.ocrStatus.mockImplementation(async () => ({
    running: false,
    stats: { done: 0, failed: 0, pending: 0, noOcrAssets: 0 },
    progress: null,
    report: null,
    error: null,
    modelsReady: true,
    missingModelFiles: [],
  }))
}

const { TagTasks } = await import('../src/pages/TagTasks')

function renderEditor() {
  // 编辑器开合由 URL 驱动（?open=new-task）：上一个用例保存成功会把这个键从地址里 replace 掉，
  // 这里每例重置地址，保证对话框一定打开。
  window.history.replaceState(null, '', '/tag-tasks?open=new-task')
  return render(
    <BrowserRouter>
      <TagTasks />
    </BrowserRouter>,
  )
}

const promptBox = () => screen.getByLabelText(/^提示词/) as HTMLTextAreaElement
const nameBox = () => screen.getByLabelText(/^任务名/) as HTMLInputElement

beforeEach(() => {
  stubStatus()
  apiStub.tagTaskCreate.mockReset().mockImplementation(async () => ({ id: 1 }))
  apiStub.tagTaskUpdate.mockReset().mockImplementation(async () => ({}))
})

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
})

describe('统一标签 · 任务创建引导与保存强校验', () => {
  it('新建任务时 prompt 预填输出格式段（数组 + _ 连接）', async () => {
    renderEditor()
    const box = await screen.findByLabelText(/^提示词/)
    expect((box as HTMLTextAreaElement).value).toContain('输出 JSON 字符串数组')
    expect((box as HTMLTextAreaElement).value).toContain('_')
    expect((box as HTMLTextAreaElement).value).toBe(TAG_PROMPT_FORMAT_CLAUSE)
  })

  it('prompt 未声明输出格式：保存默认被阻止（弹确认，取消 = 不落库）', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false)
    renderEditor()
    fireEvent.change(await screen.findByLabelText(/^任务名/), { target: { value: '拍摄地点' } })
    fireEvent.change(screen.getByLabelText(/^提示词/), {
      target: { value: '这张照片的拍摄地点是哪里？用简短中文回答' },
    })
    fireEvent.click(screen.getByText('保存'))
    await waitFor(() => expect(confirmSpy).toHaveBeenCalledTimes(1))
    expect(confirmSpy.mock.calls[0][0]).toContain('未声明输出格式')
    expect(apiStub.tagTaskCreate).not.toHaveBeenCalled()
  })

  it('同一 prompt 用户确认后放行：正常调创建接口', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true)
    renderEditor()
    fireEvent.change(await screen.findByLabelText(/^任务名/), { target: { value: '拍摄地点' } })
    fireEvent.change(screen.getByLabelText(/^提示词/), {
      target: { value: '这张照片的拍摄地点是哪里？用简短中文回答' },
    })
    fireEvent.click(screen.getByText('保存'))
    await waitFor(() => expect(apiStub.tagTaskCreate).toHaveBeenCalledTimes(1))
    expect(apiStub.tagTaskCreate).toHaveBeenCalledWith('拍摄地点', '这张照片的拍摄地点是哪里？用简短中文回答', true)
  })

  it('prompt 已声明格式（数组 + _）：不弹确认直接保存', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false)
    renderEditor()
    const declared = '这张照片的拍摄地点是哪里？输出为离散标签数组，多词用 _ 连接'
    fireEvent.change(await screen.findByLabelText(/^任务名/), { target: { value: '拍摄地点' } })
    fireEvent.change(screen.getByLabelText(/^提示词/), { target: { value: declared } })
    fireEvent.click(screen.getByText('保存'))
    await waitFor(() => expect(apiStub.tagTaskCreate).toHaveBeenCalledTimes(1))
    expect(confirmSpy).not.toHaveBeenCalled()
  })

  it('「插入格式要求」按钮把格式段补进当前 prompt（一键补齐）', async () => {
    renderEditor()
    const box = (await screen.findByLabelText(/^提示词/)) as HTMLTextAreaElement
    // 模拟用户先清空再写提问（旧习惯写法）
    fireEvent.change(box, { target: { value: '这是什么品牌型号？' } })
    fireEvent.click(screen.getByTestId('tagtask-insert-format'))
    await waitFor(() => expect(promptBox().value).toContain('这是什么品牌型号？'))
    expect(promptBox().value).toContain(TAG_PROMPT_FORMAT_CLAUSE)
    expect(promptDeclares(promptBox().value)).toBe(true)
    // 格式段已在 prompt 里 → 按钮收起（不重复插入）
    expect(screen.queryByTestId('tagtask-insert-format')).toBeNull()
  })
})

function promptDeclares(p: string): boolean {
  return p.includes('数组') && (p.includes('_') || p.includes('下划线'))
}

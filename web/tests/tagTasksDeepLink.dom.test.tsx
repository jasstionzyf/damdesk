// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/tag-tasks?open=edit-task&task=7"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'

/**
 * 卡 task：TagTasks 编辑对话框深链冷加载打不开（违反 URL-STATE-1 自述契约）。
 *
 * 旧实现（appliedOpen ref）：首轮 effect 在 taskList 尚未从 API 返回时就记下
 * `edit-task:7`，首次 tagTasks 响应到达后 effect 重跑时同 key 被 guard 挡住 →
 * setEditing(null) 之后不再重放 → 对话框永远不出现。
 *
 * 本测试用「首次 tagTasks 响应挂起」模拟真实冷加载，断言：
 *  1. 首帧（taskList 仍为空）不弹对话框、不抛错；
 *  2. 首次响应到达后对话框出现，并带原任务 name/prompt；
 *  3. 之后刷新 taskList（服务端内容变了）不冲掉用户正在输入的草稿；
 *  4. 反向：URL 指向不存在的 task → 不开对话框、不抛错。
 */

const apiStub = vi.hoisted(() => ({
  tagStatus: vi.fn(),
  tagTasks: vi.fn(),
  tagFailed: vi.fn(),
  tagCustomFailed: vi.fn(),
  ocrStatus: vi.fn(),
  tagTaskCreate: vi.fn(),
  tagTaskUpdate: vi.fn(),
  tagTaskDelete: vi.fn(),
  tagTaskRun: vi.fn(),
  tagStart: vi.fn(),
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

import { TagTasks } from '../src/pages/TagTasks'

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

const TASK_7 = {
  id: 7,
  name: '拍摄地点',
  prompt: '这张照片的拍摄地点是哪里？输出为离散标签数组',
  enabled: true,
  createdAt: 0,
  updatedAt: 0,
  count: 0,
}

/** 手动控制首次 tagTasks 响应（模拟 API 冷启动慢于首帧渲染） */
function deferredTasks() {
  let resolveFirst: (v: { items: typeof TASK_7[] }) => void = () => {}
  let first = true
  apiStub.tagTasks.mockImplementation(
    () =>
      new Promise<{ items: typeof TASK_7[] }>((resolve) => {
        if (first) {
          first = false
          resolveFirst = resolve
        } else {
          resolve({ items: [TASK_7] })
        }
      }),
  )
  return (items: typeof TASK_7[]) => resolveFirst({ items })
}

function renderPage(initialEntry = '/tag-tasks?open=edit-task&task=7') {
  return render(
    <MemoryRouter initialEntries={[initialEntry]}>
      <Routes>
        <Route path="/tag-tasks" element={<TagTasks />} />
      </Routes>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  stubStatus()
})

afterEach(() => {
  cleanup()
  vi.clearAllMocks()
})

describe('TagTasks 编辑对话框深链冷加载（task）', () => {
  it('首帧 taskList 为空不弹；到达后对话框出现并带原 prompt', async () => {
    const resolveFirst = deferredTasks()
    renderPage()

    // 首帧（taskList 空、API 还没回）：对话框不出现，也不抛错
    expect(screen.queryByTestId('tagtask-editor')).toBeNull()

    // 冷加载响应到达 → 对话框必须重放出来（旧代码在这里被 guard 挡住）
    resolveFirst([TASK_7])
    await waitFor(() => expect(screen.getByTestId('tagtask-editor')).toBeTruthy())
    expect(screen.getByText('编辑任务')).toBeTruthy()
    expect(screen.getByDisplayValue('拍摄地点')).toBeTruthy()
    expect(screen.getByDisplayValue(TASK_7.prompt)).toBeTruthy()
  })

  it('刷新 taskList（服务端内容变化）不冲掉用户正在编辑的草稿', async () => {
    const resolveFirst = deferredTasks()
    const { container } = renderPage()
    resolveFirst([TASK_7])
    await waitFor(() => expect(screen.getByTestId('tagtask-editor')).toBeTruthy())

    const nameInput = screen.getByDisplayValue('拍摄地点') as HTMLInputElement
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      'value',
    )!.set!
    setter.call(nameInput, '用户改过的名字')
    nameInput.dispatchEvent(new Event('input', { bubbles: true }))
    await waitFor(() => expect(screen.getByDisplayValue('用户改过的名字')).toBeTruthy())

    // 服务端把任务名改了 + 触发一次刷新 → 草稿仍是用户的输入
    apiStub.tagTasks.mockImplementation(async () => ({
      items: [{ ...TASK_7, name: '服务端另改的名字' }],
    }))
    window.dispatchEvent(new Event('dam:library-changed'))
    await waitFor(() =>
      expect((container.querySelector('input') as HTMLInputElement).value).toBe('用户改过的名字'),
    )
  })

  it('反链：URL 指向不存在的 task → 不开对话框、不抛错', async () => {
    apiStub.tagTasks.mockImplementation(async () => ({ items: [TASK_7] }))
    renderPage('/tag-tasks?open=edit-task&task=999')
    await waitFor(() => expect(apiStub.tagTasks).toHaveBeenCalled())
    expect(screen.queryByTestId('tagtask-editor')).toBeNull()
  })
})

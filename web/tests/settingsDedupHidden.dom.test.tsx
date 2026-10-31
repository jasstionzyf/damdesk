// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

/**
 * KI-1 / KI-6 / KI-4 前端回归（卡 task）：
 *
 * KI-1+KI-6 —— 设置页**不出现去重塔的任何形态**（下拉选项 / 警示文案 / 向量索引区的 dedup 行）。
 *   裁决（2026-10-18）：设置界面不展示去重模型信息，去重相关界面一律不出现模型名/向量/嵌入字样。
 *   本用例故意让后端多回一个历史字段 `imageOnlyEmbedders`（旧口径会把它渲染成 disabled 选项 +
 *   「仅图像向量，不能用于文搜图」说明），断言页面文本里**一个 dinov3 都没有** —— 这条就是防复发钉子。
 *   后端口径：PUT /api/settings/inference 仍明确拒绝仅图像塔（400 no_text_modality，不静默降级）。
 *
 * KI-4 —— base_url placeholder 按形态分两套文案：
 *   桌面（localInfer=true）= 自建 vLLM / 云 API 的 /v1 地址（不再写容器才有的 host.docker.internal）；
 *   容器（localInfer=false）= 才写 host.docker.internal，并注明「仅容器形态默认」。
 */

const { apiMock } = vi.hoisted(() => {
  const settings = {
    active_embedder: 'openai-compat-embedding',
    model_base_url: '',
    auto_download: true,
    embedding: {
      base_url: 'http://host.docker.internal:8090/v1',
      api_key: '',
      model: 'clip-vit-l14',
      dims: 768,
      timeout_secs: 120,
      proxy: '',
    },
    tag: {
      provider: 'damai',
      base_url: 'http://host.docker.internal:8091/v1',
      model: 'qwen3.5-4b',
      api_key: '',
      enabled: false,
      prompt_template: '',
      max_tokens: 2048,
      timeout_ms: 60000,
      proxy: null,
    },
  }
  return {
    apiMock: {
      inference: vi.fn(async () => ({
        settings,
        embedders: [
          {
            name: 'siglip2-base-p224-onnx',
            kind: 'embedding',
            dims: 768,
            modalities: ['image', 'text'],
            provider: 'onnx_siglip',
            preprocess: null,
          },
          {
            name: 'openai-compat-embedding',
            kind: 'embedding',
            dims: 0,
            modalities: ['image', 'text'],
            provider: 'openai_compat',
            preprocess: null,
          },
        ],
        // 故意保留历史字段：前端不得再消费它（KI-6）
        imageOnlyEmbedders: [
          {
            name: 'dinov3-vits16-onnx',
            kind: 'embedding',
            dims: 384,
            modalities: ['image'],
            provider: 'onnx_dinov3',
            preprocess: null,
          },
        ],
        capabilities: { localInfer: false, remoteProvider: true, note: 'web 构建不含本地 ONNX 推理' },
      })),
      saveInference: vi.fn(async (s: unknown) => ({ ok: true, settings: s })),
      testEmbed: vi.fn(async () => ({ ok: true, dims: 768, endpointModel: 'clip-vit-l14', modelKey: 'k', baseUrl: 'u' })),
      searchStatus: vi.fn(async () => null),
      ocrStatus: vi.fn(async () => ({
        running: false,
        stats: { done: 0, failed: 0, pending: 0, noOcrAssets: 0 },
        progress: null,
        report: null,
        error: null,
        modelsReady: true,
        missingModelFiles: [],
      })),
      ocrStart: vi.fn(async () => ({ enqueued: 0, mode: 'pending', limit: 0 })),
      ocrCancel: vi.fn(async () => ({ cancelling: true })),
      embedStatus: vi.fn(async () => null),
      embedFailed: vi.fn(async () => ({ items: [] })),
      embedStart: vi.fn(async () => ({ queued: 0, model: 'm' })),
      embedCancel: vi.fn(async () => ({ ok: true })),
      embedRetryFailed: vi.fn(async () => ({ reset: 0 })),
      tagPresets: vi.fn(async () => ({ presets: [] })),
      tagTest: vi.fn(async () => ({ ok: true })),
      tagScan: vi.fn(async () => ({ models: [] })),
      exifSettings: vi.fn(async () => null),
      exifStatus: vi.fn(async () => null),
      models: vi.fn(async () => ({ root: '/models', baseUrl: '', downloadable: [], models: [] })),
      modelDownload: vi.fn(async () => ({})),
      modelDownloadStatus: vi.fn(async () => ({
        running: false,
        model: null,
        status: null,
        doneBytes: 0,
        totalBytes: 0,
        completedFiles: 0,
        totalFiles: 0,
        error: null,
        outcome: null,
        startedAt: null,
        finishedAt: null,
      })),
      capabilities: vi.fn(async () => ({
        isDesktop: false,
        storeMode: 'server-volume',
        writableRoots: [],
        roots: [],
        features: {
          moveAsset: false,
          renameAsset: false,
          relink: false,
          openInOS: false,
          physicalDelete: false,
          copyInImport: false,
          scanExternalPath: false,
        },
        featureReasons: {},
      })),
    },
  }
})

vi.mock('../src/lib/api', () => ({
  api: apiMock,
  DEFAULT_TAG_MAX_TOKENS: 2048,
  formatSize: (n: number) => `${n}B`,
}))

vi.mock('../src/lib/tasks', () => ({
  useTasks: () => ({
    tasks: [],
    runningCount: 0,
    startEmbed: vi.fn(),
    startImport: vi.fn(),
    startTag: vi.fn(),
    cancel: vi.fn(),
    retry: vi.fn(),
    dismiss: vi.fn(),
  }),
}))

vi.mock('../src/lib/toast', () => ({
  useToast: () => ({ toast: vi.fn(), dismiss: vi.fn() }),
  ToastProvider: ({ children }: { children: unknown }) => children,
}))

import { Settings } from '../src/pages/Settings'

/** 等设置页首个异步数据落地（活跃 embedder 下拉渲染出来） */
async function activeSelect(): Promise<HTMLSelectElement> {
  return waitFor(() => {
    const sels = Array.from(document.querySelectorAll('select'))
    const hit = sels.find((s) =>
      Array.from(s.options).some((o) => o.value === 'openai-compat-embedding'),
    ) as HTMLSelectElement | undefined
    if (!hit) throw new Error('活跃 embedder 下拉还没渲染')
    return hit
  })
}

async function baseUrlInput(): Promise<HTMLInputElement> {
  return waitFor(() => {
    const el = document.querySelector('[data-testid="embed-base-url"]') as HTMLInputElement | null
    if (!el) throw new Error('base_url 输入框还没渲染')
    return el
  })
}

function renderSettings() {
  return render(
    <MemoryRouter>
      <Settings />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  vi.clearAllMocks()
})

afterEach(() => {
  cleanup()
})

describe('设置页不展示去重模型信息（KI-1 + KI-6）', () => {
  it('下拉只含文搜图可用 spec；历史 imageOnlyEmbedders 字段不再渲染成选项/警示', async () => {
    renderSettings()
    const sel = await activeSelect()

    const values = Array.from(sel.options).map((o) => o.value)
    expect(values).toContain('openai-compat-embedding')
    expect(values.some((v) => v.toLowerCase().includes('dinov3'))).toBe(false)
    expect(sel.value).toBe('openai-compat-embedding')

    // 文案层（含 disabled 选项说明、警示条）不得出现去重塔任何写法
    const text = (document.body.textContent ?? '').toLowerCase()
    expect(text).not.toContain('dinov3')
    expect(text).not.toContain('仅图像向量')
    expect(text).not.toContain('不能用于文搜图')
  })
})

describe('base_url placeholder 按形态分支（KI-4）', () => {
  it('容器形态（localInfer=false）：写 host.docker.internal 并注明「仅容器形态默认」', async () => {
    renderSettings()
    const input = await baseUrlInput()
    expect(input.placeholder).toContain('host.docker.internal:8090/v1')
    expect(input.placeholder).toContain('仅容器形态默认')
  })

  it('桌面形态（localInfer=true）：给自建 vLLM / 云 API 的 /v1 地址，不出现 host.docker.internal', async () => {
    const base = await apiMock.inference()
    apiMock.inference.mockResolvedValue({
      ...base,
      capabilities: { localInfer: true, remoteProvider: true, note: '本构建含本地 ONNX 推理' },
    })
    renderSettings()
    const input = await baseUrlInput()
    expect(input.placeholder).toContain('自建 vLLM')
    expect(input.placeholder).toContain('/v1')
    expect(input.placeholder).not.toContain('host.docker.internal')
  })
})

// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

/**
 * 打标设置 max_tokens 可编辑性回归（DT-A，2026-10-19）。
 *
 * 去重塔不出现在设置页（KI-1/KI-6）由 settingsDedupHidden.dom.test.tsx 钉住，本用例只管
 * 打标参数：默认 2048 / 步进 512 / 可键入覆盖 / 快捷档位一键设值。
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
        capabilities: { localInfer: false, remoteProvider: true, note: 'web 构建不含本地 ONNX 推理' },
      })),
      saveInference: vi.fn(async (s: unknown) => ({ ok: true, settings: s })),
      testEmbed: vi.fn(async () => ({ ok: true, dims: 768, endpointModel: 'clip-vit-l14', modelKey: 'k', baseUrl: 'u' })),
      searchStatus: vi.fn(async () => null),
      // 卡 t_13a2b66b：OCR 区块迁入设置页 → 设置页要读 /api/ocr/status（就绪态）
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
      // 卡 t_6e8888ca：设置页新增「模型文件（可选下载）」区块（本用例只关心 max_tokens 输入）
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
      // KI-10（t_a60efa81）起设置页有「库根」区块，走 GET /api/capabilities；
      // 本用例只关心 embedder 下拉，能力位给一份「网页形态 / 无库根」的最小快照。
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

beforeEach(() => {
  vi.clearAllMocks()
})

afterEach(() => {
  cleanup()
})

describe('DT-A 打标设置 max_tokens 可编辑性（2026-10-19 用户报告修复）', () => {
  it('默认 2048 / 步进 512 / 可键入覆盖 / 快捷档位一键设值', async () => {
    // 单次 render 完成全部断言：Settings 页面 DOM 很大，jsdom 下多次 render 会把 worker 堆打爆
    render(
      <MemoryRouter>
        <Settings />
      </MemoryRouter>,
    )
    const input = (await waitFor(() => {
      const el = document.querySelector('[data-testid="tag-max-tokens"]') as HTMLInputElement | null
      if (!el) throw new Error('max_tokens 输入框还没渲染')
      return el
    })) as HTMLInputElement

    // 1) 默认值 + 箭头步进（改前 step=1，从 2048 调到 4096 要点 2048 次）
    expect(input.value).toBe('2048')
    expect(input.step).toBe('512')
    expect(input.min).toBe('0')
    expect(input.max).toBe('65536')

    // 2) 可直接键入覆盖
    fireEvent.change(input, { target: { value: '6144' } })
    expect(input.value).toBe('6144')
    expect(screen.getByText(/6144（设置页覆盖）/)).toBeTruthy()

    // 3) 快捷档位一键设值
    fireEvent.click(document.querySelector('[data-testid="tag-max-tokens-preset-4096"]')!)
    expect(input.value).toBe('4096')
    fireEvent.click(document.querySelector('[data-testid="tag-max-tokens-preset-8192"]')!)
    expect(input.value).toBe('8192')
    fireEvent.click(document.querySelector('[data-testid="tag-max-tokens-preset-0"]')!)
    expect(input.value).toBe('0')
    // 0 = 用默认值：文案回到「2048（默认）」
    expect(screen.getByText(/2048（默认）/)).toBeTruthy()
  })
})

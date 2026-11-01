// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

/**
 * 卡 t_6e8888ca 前端回归：设置页「模型文件（可选下载）」。
 *
 * 背景：`lama-fp32`（局部重绘 208MB）不内置，之前**没有任何下载入口** —— 用户只能猜目录
 * 手工拖 onnx 进去，A5 局部重绘因此长期是「判定能跑、修复不能跑」。
 *
 * 钉住的用户可见口径：
 *  1) 未下载 → 显示「未下载」+ 体积 + 下载按钮 + 缺件绝对路径（手放也有据可依）；
 *  2) 下载中 → 显示百分比进度，按钮不可重复点（后端也是单飞）；
 *  3) 失败 → 如实显示后端错误 + 重试按钮，状态**不能**变成已就绪；
 *  4) 已就绪 → 只显示「已就绪」，不再显示下载/缺件清单（不制造重复动作）。
 */

const { apiMock, modelState } = vi.hoisted(() => {
  const modelState = {
    ready: false,
    running: false,
    status: 'failed' as 'downloading' | 'done' | 'failed' | null,
    doneBytes: 0,
    error: null as string | null,
  }
  const LAMA_PATH =
    '/home/u/.local/share/com.soujpg.dam.desktop/models/lama-fp32/lama-fp32.onnx'
  const view = () => ({
    root: '/home/u/.local/share/com.soujpg.dam.desktop/models',
    baseUrl: 'https://mirror.example.com/dam-models',
    downloadable: ['lama-fp32'],
    models: [
      {
        name: 'siglip2-base-p224-onnx',
        downloadable: false,
        ready: true,
        presentBytes: 800_000_000,
        totalBytes: 800_000_000,
        missingPaths: [],
        files: [],
      },
      {
        name: 'lama-fp32',
        downloadable: true,
        ready: modelState.ready,
        presentBytes: modelState.ready ? 208_044_816 : 0,
        totalBytes: 208_044_816,
        missingPaths: modelState.ready ? [] : [LAMA_PATH],
        files: [],
      },
    ],
  })
  return {
    modelState,
    apiMock: {
      models: vi.fn(async () => view()),
      modelDownload: vi.fn(async (name: string) => ({
        ok: true,
        model: name,
        baseUrl: 'https://mirror.example.com/dam-models',
        root: '/home/u/.local/share/com.soujpg.dam.desktop/models',
        totalBytes: 208_044_816,
        files: 1,
      })),
      modelDownloadStatus: vi.fn(async () => ({
        running: modelState.running,
        model: modelState.status ? 'lama-fp32' : null,
        status: modelState.status,
        doneBytes: modelState.doneBytes,
        totalBytes: 208_044_816,
        completedFiles: modelState.doneBytes > 0 ? 1 : 0,
        totalFiles: 1,
        error: modelState.error,
        outcome: null,
        startedAt: 1,
        finishedAt: 2,
      })),
      // 设置页其余区块的最小可渲染载荷
      inference: vi.fn(async () => ({
        settings: {
          active_embedder: 'siglip2-base-p224-onnx',
          model_base_url: '',
          auto_download: true,
          embedding: {
            base_url: '',
            api_key: '',
            model: '',
            dims: 0,
            timeout_secs: 120,
            proxy: '',
          },
          tag: {
            provider: 'damai',
            base_url: '',
            model: '',
            api_key: '',
            enabled: false,
            prompt_template: '',
            max_tokens: 2048,
            timeout_ms: 60000,
            proxy: null,
          },
        },
        embedders: [],
        imageOnlyEmbedders: [],
        capabilities: { localInfer: true, remoteProvider: true, note: '' },
      })),
      saveInference: vi.fn(async (s: unknown) => ({ ok: true, settings: s })),
      testEmbed: vi.fn(async () => ({ ok: true, dims: 768, endpointModel: 'm', modelKey: 'k', baseUrl: 'u' })),
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
      capabilities: vi.fn(async () => ({
        isDesktop: true,
        storeMode: 'appdata',
        writableRoots: [],
        roots: [],
        features: {
          moveAsset: true,
          renameAsset: true,
          relink: true,
          openInOS: true,
          physicalDelete: true,
          copyInImport: true,
          scanExternalPath: true,
        },
        featureReasons: {},
      })),
    },
  }
})

vi.mock('../src/lib/api', () => ({
  api: apiMock,
  DEFAULT_TAG_MAX_TOKENS: 2048,
  formatSize: (n: number) => `${Math.round(n / 1024 / 1024)}MB`,
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

function renderSettings() {
  render(
    <MemoryRouter>
      <Settings />
    </MemoryRouter>,
  )
}

beforeEach(() => {
  vi.clearAllMocks()
  modelState.ready = false
  modelState.running = false
  modelState.status = null
  modelState.doneBytes = 0
  modelState.error = null
})

afterEach(() => {
  cleanup()
})

describe('设置页 · 模型文件（可选下载）（卡 t_6e8888ca）', () => {
  it('未下载：给体积 + 下载按钮 + 缺件绝对路径，内置模型不出现', async () => {
    renderSettings()
    await waitFor(() => expect(screen.getByTestId('model-row-lama-fp32')).toBeTruthy())
    expect(screen.getByTestId('model-state-lama-fp32').textContent).toContain('未下载')
    const btn = screen.getByTestId('model-download-lama-fp32')
    expect(btn.textContent).toContain('下载')
    expect(btn.textContent).toContain('198MB') // 208044816 B ≈ 198MB
    const missing = screen.getByTestId('model-missing-lama-fp32')
    expect(missing.textContent).toContain('/models/lama-fp32/lama-fp32.onnx')
    // 内置的 siglip 不在「可选下载」清单里（一能力一处，别让用户重复下）
    expect(screen.queryByTestId('model-row-siglip2-base-p224-onnx')).toBeNull()
  })

  it('下载中：显示百分比进度，按钮禁用（后端也是单飞）', async () => {
    modelState.running = true
    modelState.status = 'downloading'
    modelState.doneBytes = 83_217_926 // ~40%
    renderSettings()
    await waitFor(() =>
      expect(screen.getByTestId('model-state-lama-fp32').textContent).toContain('下载中 40%'),
    )
    expect(screen.getByTestId('model-progress-lama-fp32')).toBeTruthy()
    const btn = screen.getByTestId('model-download-lama-fp32') as HTMLButtonElement
    expect(btn.disabled).toBe(true)
    expect(btn.textContent).toContain('下载中')
  })

  it('失败：如实显示后端错误 + 重试按钮，状态仍为未下载（不装成就绪）', async () => {
    modelState.status = 'failed'
    modelState.error = 'GET lama-fp32.onnx: status code 404'
    renderSettings()
    await waitFor(() => expect(screen.getByTestId('model-error-lama-fp32')).toBeTruthy())
    expect(screen.getByTestId('model-error-lama-fp32').textContent).toContain('status code 404')
    expect(screen.getByTestId('model-state-lama-fp32').textContent).toContain('未下载')
    expect(screen.getByTestId('model-retry-lama-fp32')).toBeTruthy()
  })

  it('已就绪：只显示「已就绪」，不再出现下载按钮与缺件清单', async () => {
    modelState.ready = true
    modelState.status = 'done'
    renderSettings()
    await waitFor(() =>
      expect(screen.getByTestId('model-state-lama-fp32').textContent).toContain('已就绪'),
    )
    expect(screen.queryByTestId('model-download-lama-fp32')).toBeNull()
    expect(screen.queryByTestId('model-missing-lama-fp32')).toBeNull()
  })

  it('点下载调后端 /api/models/{name}/download', async () => {
    renderSettings()
    await waitFor(() => expect(screen.getByTestId('model-download-lama-fp32')).toBeTruthy())
    fireEvent.click(screen.getByTestId('model-download-lama-fp32'))
    await waitFor(() => expect(apiMock.modelDownload).toHaveBeenCalledWith('lama-fp32'))
  })
})

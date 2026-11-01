// @vitest-environment jsdom
/**
 * GH-9（卡 t_a148715b）模型分发前端：设置页模型区三件新能力的 DOM 验收。
 *
 *  1) 聚合首屏：missingCount/missingBytes → 「下载全部可选模型（N 个 · X MB）」按钮；
 *     点击后按缺件顺序逐个 modelDownload（已就绪的跳过），等单飞收工再发下一个；
 *  2) 每行带后端 model_info（体积/许可/来源），已就绪行显示「已就绪」且无下载/导入按钮；
 *  3) 「从文件夹导入」：非桌面形态退化为 window.prompt 手填路径 → modelDownload(name, file://路径)。
 *
 * 后端聚合/409/file:// 落盘契约在 crates/app-web/tests/t_a148715b_model_distribution.rs 钉。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

const { apiMock, statusQueue } = vi.hoisted(() => {
  const statusQueue: { running: boolean; status: string | null; model: string | null }[] = []
  const apiMock = {
    models: vi.fn(async () => ({
      root: '/home/u/.local/share/com.soujpg.dam.desktop/models',
      baseUrl: 'https://huggingface.co/jasstionzyf/damdesk-models/resolve/main',
      sources: [
        'https://huggingface.co/jasstionzyf/damdesk-models/resolve/main',
        'https://hf-mirror.com/jasstionzyf/damdesk-models/resolve/main',
      ],
      sourceMode: 'default',
      downloadable: ['u2net', 'realesr-general-x4v3', 'lama-fp32'],
      missingCount: 2,
      missingBytes: 4_866_417 + 208_044_816,
      optionalBytes: 175_997_641 + 4_866_417 + 208_044_816,
      models: [
        {
          name: 'u2net',
          downloadable: true,
          ready: true,
          presentBytes: 175_997_641,
          totalBytes: 175_997_641,
          license: 'Apache-2.0',
          source: 'rembg 同源权重（GitHub release / HF tomjackson2023/rembg）',
          missingPaths: [],
          files: [],
        },
        {
          name: 'realesr-general-x4v3',
          downloadable: true,
          ready: false,
          presentBytes: 0,
          totalBytes: 4_866_417,
          license: 'BSD-3-Clause',
          source: 'Real-ESRGAN 官方权重（ONNX 再打包）',
          missingPaths: ['/home/u/.local/share/com.soujpg.dam.desktop/models/realesr-general-x4v3/realesr-general-x4v3.onnx'],
          files: [],
        },
        {
          name: 'lama-fp32',
          downloadable: true,
          ready: false,
          presentBytes: 0,
          totalBytes: 208_044_816,
          license: 'Apache-2.0',
          source: 'Carve/LaMa-ONNX 导出（FP32）',
          missingPaths: ['/home/u/.local/share/com.soujpg.dam.desktop/models/lama-fp32/lama-fp32.onnx'],
          files: [],
        },
      ],
    })),
    modelDownload: vi.fn(async (name: string) => ({
      ok: true,
      model: name,
      baseUrl: 'u',
      root: 'r',
      totalBytes: 1,
      files: 1,
    })),
    modelDownloadStatus: vi.fn(async () => {
      // 首屏刷新读到空闲；只有发起下载后才按队列吐「下载中 → 收工」（downloadAll 轮询吃它）
      const idle = {
        running: false,
        status: 'done',
        model: null,
        doneBytes: 0,
        totalBytes: 0,
        completedFiles: 0,
        totalFiles: 0,
        error: null,
        outcome: null,
        startedAt: 1,
        finishedAt: 2,
      }
      if (apiMock.modelDownload.mock.calls.length === 0) return idle
      return statusQueue.shift() ?? idle
    }),
    inference: vi.fn(async () => ({
      settings: {
        active_embedder: 'siglip2-base-p224-onnx',
        model_base_url: '',
        auto_download: true,
        embedding: { base_url: '', api_key: '', model: '', dims: 0, timeout_secs: 120, proxy: '' },
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
  }
  return { apiMock, statusQueue }
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

import { LocaleProvider } from '../src/i18n'
import { Settings } from '../src/pages/Settings'

beforeEach(() => {
  vi.clearAllMocks()
  statusQueue.length = 0
  window.localStorage.clear()
  window.prompt = vi.fn(() => '/tmp/model-src')
})

afterEach(() => {
  cleanup()
  window.localStorage.clear()
})

function renderSettings() {
  return render(
    <LocaleProvider>
      <MemoryRouter initialEntries={['/settings']}>
        <Settings />
      </MemoryRouter>
    </LocaleProvider>,
  )
}

describe('GH-9 模型分发（设置页）', () => {
  it('首屏聚合：缺 2 个可选模型 → 「下载全部（2 个 · 203MB）」；点击后跳过已就绪、逐个下载', async () => {
    // 第一个模型下载中 → 第二个快照收工（downloadAll 轮询至少各吃一条）
    statusQueue.push({ running: true, status: 'downloading', model: 'realesr-general-x4v3' })
    renderSettings()
    await waitFor(() => expect(screen.getByTestId('model-row-u2net')).toBeTruthy())

    const btn = screen.getByTestId('models-download-all')
    expect(btn.textContent).toContain('2')
    expect(btn.textContent).toContain('203MB')

    btn.click()
    await waitFor(() => {
      const names = apiMock.modelDownload.mock.calls.map((c) => c[0])
      expect(names).toContain('realesr-general-x4v3')
      expect(names).toContain('lama-fp32')
    })
    // 已就绪的 u2net 不重下（不把「点过下载」当缺件）
    const names = apiMock.modelDownload.mock.calls.map((c) => c[0])
    expect(names).not.toContain('u2net')
    // 顺序：按行序（realesr 在前）
    expect(names.indexOf('realesr-general-x4v3')).toBeLessThan(names.indexOf('lama-fp32'))
  })

  it('每行带许可/来源；已就绪行显示「已就绪」且没有下载/导入按钮', async () => {
    renderSettings()
    await waitFor(() => expect(screen.getByTestId('model-row-u2net')).toBeTruthy())

    expect(screen.getByTestId('model-state-u2net').textContent).toContain('已就绪')
    expect(screen.queryByTestId('model-download-u2net')).toBeNull()
    expect(screen.queryByTestId('model-import-u2net')).toBeNull()
    expect(screen.getByTestId('model-license-realesr-general-x4v3').textContent).toBe('BSD-3-Clause')
    // 缺件行有导入按钮（手动兜底入口常驻）
    expect(screen.getByTestId('model-import-realesr-general-x4v3')).toBeTruthy()
    expect(screen.getByTestId('models-source-mirrors').textContent).toContain('huggingface.co')
    expect(screen.getByTestId('models-source-mirrors').textContent).toContain('hf-mirror.com')
  })

  it('从文件夹导入：web 形态退化为手填路径，file:// 前缀补齐后走下载通道', async () => {
    renderSettings()
    await waitFor(() => expect(screen.getByTestId('model-row-lama-fp32')).toBeTruthy())

    ;(screen.getByTestId('model-import-lama-fp32') as HTMLButtonElement).click()
    await waitFor(() => {
      expect(apiMock.modelDownload).toHaveBeenCalledWith(
        'lama-fp32',
        'file:///tmp/model-src',
      )
    })
  })
})

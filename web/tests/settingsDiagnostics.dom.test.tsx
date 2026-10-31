// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

/**
 * 卡 task（#9 设置页「诊断」折叠区）。验收口径：
 *  1) 默认收起 —— 收起态**一个诊断类请求都不发**（/api/search/status、/api/library/stats、
 *     /api/app-info 调用次数为 0），设置页其余分区照常渲染；
 *  2) 展开后是真实值 —— 向量表逐模型行数、当前模型（含维度）、库大小（张数 + 字节）、版本；
 *  3) 后端全挂（断网断言）时：收起态页面不受影响，展开态只在诊断区内报失败，不炸页面。
 *
 * 可编辑项（库根 / provider / EXIF / OCR）在上方，诊断不承载任何写操作 —— 本测试只读。
 */

const SEARCH = {
  activeModel: 'siglip2-base-p224-onnx',
  activeDims: 768,
  activeRows: 220147,
  searchable: true,
  models: [
    { model: 'siglip2-base-p224-onnx', dims: 768, table: 'vec_siglip2', rows: 220147, active: true },
    { model: 'dinov3-vits16-onnx', dims: 384, table: 'vec_dinov3', rows: 55, active: false },
  ],
  emb: { none: 0, pending: 0, done: 220147, failed: 0 },
  semantic: { done: 220147, failed: 0, pending: 0, remaining: 0, vectors: 220147 },
  otherModelVectors: true,
  similarMinScore: 0,
  similarMinScoreCalibrated: false,
}

const STATS = {
  assets: 219869,
  missing: 3,
  tagged: 180000,
  totalBytes: 5_000_000_000,
  byMediaType: [{ mediaType: 'image', count: 219869 }],
  missingCheckedAt: 1750000000,
  missingStale: false,
  missingUnreadable: 0,
  broken: 0,
  missingStaleSecs: 86400,
}

const APP = {
  name: 'DAMDesk',
  version: '0.9.7',
  dataDir: 'C:\\Users\\me\\AppData\\Roaming\\com.soujpg.dam.desktop',
  modelsRoot: 'C:\\Users\\me\\AppData\\Roaming\\com.soujpg.dam.desktop\\models',
  registryModels: [],
}

const INFERENCE = {
  settings: {
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
  },
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
}

const CAPS = {
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
}

const OCR = {
  running: false,
  stats: { done: 12, failed: 0, pending: 0, noOcrAssets: 4 },
  progress: null,
  report: null,
  error: null,
  modelsReady: true,
  missingModelFiles: [],
}

const { apiMock } = vi.hoisted(() => ({
  apiMock: {
    inference: vi.fn(),
    saveInference: vi.fn(),
    testEmbed: vi.fn(),
    embedStatus: vi.fn(),
    embedFailed: vi.fn(),
    embedStart: vi.fn(),
    embedCancel: vi.fn(),
    embedRetryFailed: vi.fn(),
    tagPresets: vi.fn(),
    tagTest: vi.fn(),
    tagScan: vi.fn(),
    exifSettings: vi.fn(),
    exifSettingsSave: vi.fn(),
    exifStatus: vi.fn(),
    exifBackfill: vi.fn(),
    capabilities: vi.fn(),
    ocrStatus: vi.fn(),
    ocrStart: vi.fn(),
    ocrCancel: vi.fn(),
    // 卡 task：设置页「模型文件（可选下载）」区块（本用例不关心，给空清单）
    models: vi.fn(),
    modelDownload: vi.fn(),
    modelDownloadStatus: vi.fn(),
    // 诊断三件（只有展开才该被调用）
    searchStatus: vi.fn(),
    stats: vi.fn(),
    appInfo: vi.fn(),
  },
}))

vi.mock('../src/lib/api', () => ({
  api: apiMock,
  DEFAULT_TAG_MAX_TOKENS: 2048,
  formatSize: (n: number) => {
    if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`
    if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`
    if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`
    return `${n} B`
  },
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
    retryFailed: vi.fn(),
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
  apiMock.inference.mockResolvedValue(INFERENCE)
  apiMock.embedStatus.mockResolvedValue({ running: false, progress: null, lastError: null })
  apiMock.embedFailed.mockResolvedValue({ items: [] })
  apiMock.tagPresets.mockResolvedValue({ presets: [] })
  apiMock.exifSettings.mockResolvedValue({ parseOnImport: true, gps: 'off', gpsProvider: { name: '', apiKey: '', baseUrl: '' } })
  apiMock.exifStatus.mockResolvedValue({ remaining: 0, jobDone: 0, jobFailed: 0, lastError: null })
  apiMock.capabilities.mockResolvedValue(CAPS)
  apiMock.ocrStatus.mockResolvedValue(OCR)
  apiMock.models.mockResolvedValue({ root: '/models', baseUrl: '', downloadable: [], models: [] })
  apiMock.modelDownloadStatus.mockResolvedValue({
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
  })
  apiMock.searchStatus.mockResolvedValue(SEARCH)
  apiMock.stats.mockResolvedValue(STATS)
  apiMock.appInfo.mockResolvedValue(APP)
})

afterEach(() => cleanup())

function renderSettings() {
  return render(
    <MemoryRouter>
      <Settings />
    </MemoryRouter>,
  )
}

describe('设置页「诊断」折叠区（卡 task #9）', () => {
  it('默认收起：不渲染诊断体，且不发任何一个诊断类请求', async () => {
    renderSettings()
    // 等设置页自己的分区起来（provider 表单），确认整页已 mount
    await waitFor(() => expect(apiMock.inference).toHaveBeenCalled())
    await waitFor(() => expect(apiMock.ocrStatus).toHaveBeenCalled())

    expect(document.querySelector('[data-testid="settings-diagnostics"]')).not.toBeNull()
    expect(document.querySelector('[data-testid="diag-body"]')).toBeNull()
    expect(document.querySelector('[data-testid="diag-toggle"]')?.getAttribute('aria-expanded')).toBe('false')

    // 收起态零请求（这就是「不占首屏 / 断网也不影响」的硬判据）
    expect(apiMock.searchStatus).not.toHaveBeenCalled()
    expect(apiMock.stats).not.toHaveBeenCalled()
    expect(apiMock.appInfo).not.toHaveBeenCalled()
  })

  it('展开后显示真实值：向量表逐模型行数、当前模型、库大小、版本', async () => {
    renderSettings()
    await waitFor(() => expect(apiMock.inference).toHaveBeenCalled())

    fireEvent.click(await waitFor(() => {
      const b = document.querySelector('[data-testid="diag-toggle"]')
      if (!b) throw new Error('诊断开关没渲染')
      return b
    }))

    await waitFor(() => expect(document.querySelector('[data-testid="diag-body"]')).not.toBeNull())
    await waitFor(() => expect(apiMock.appInfo).toHaveBeenCalled())

    // 版本
    await waitFor(() =>
      expect(document.querySelector('[data-testid="diag-version"]')?.textContent).toContain('v0.9.7'),
    )
    // 库大小（张数 + 人类可读字节）与当前模型（含维度）
    expect(document.querySelector('[data-testid="diag-library-size"]')?.textContent).toContain('219869 张')
    expect(document.querySelector('[data-testid="diag-library-size"]')?.textContent).toContain('4.66 GB')
    expect(document.querySelector('[data-testid="diag-active-model"]')?.textContent).toContain('siglip2-base-p224-onnx')
    expect(document.querySelector('[data-testid="diag-active-model"]')?.textContent).toContain('768')

    // 向量表：只列语义塔，行数是后端真值（不是本地猜的）
    await waitFor(() =>
      expect(document.querySelector('[data-testid="diag-rows-siglip2-base-p224-onnx"]')?.textContent).toBe(
        '220147',
      ),
    )
    // KI-6：去重塔的向量表/行数不进设置页（后端 search/status 里即使带着也不列）
    expect(document.querySelector('[data-testid="diag-rows-dinov3-vits16-onnx"]')).toBeNull()
    expect(document.querySelector('[data-testid="diag-vector-tables"]')?.textContent).not.toContain(
      'vec_dinov3',
    )
    expect((document.body.textContent ?? '').toLowerCase()).not.toContain('dinov3')

    // 展开态可以收起（折叠回去）
    fireEvent.click(document.querySelector('[data-testid="diag-toggle"]') as Element)
    await waitFor(() => expect(document.querySelector('[data-testid="diag-body"]')).toBeNull())
  })

  it('诊断接口全挂（断网）：收起态设置页照常可用，展开只在诊断区内报失败', async () => {
    apiMock.searchStatus.mockRejectedValue(new Error('network down'))
    apiMock.stats.mockRejectedValue(new Error('network down'))
    apiMock.appInfo.mockRejectedValue(new Error('network down'))

    renderSettings()
    // 上方可编辑分区正常（provider 表单已读到）
    await waitFor(() => expect(apiMock.inference).toHaveBeenCalled())
    await waitFor(() => expect(document.querySelector('[data-testid="ocr-section"]')).not.toBeNull())
    expect(document.querySelector('[data-testid="diag-body"]')).toBeNull()

    fireEvent.click(document.querySelector('[data-testid="diag-toggle"]') as Element)
    await waitFor(() =>
      expect(document.querySelector('[data-testid="diag-body"]')?.textContent).toContain('读取向量状态失败'),
    )
    // 失败时占位符是「—」，不编造数字
    expect(document.querySelector('[data-testid="diag-version"]')?.textContent).toContain('—')
    expect(document.querySelector('[data-testid="diag-library-size"]')?.textContent).toContain('—')
    expect(document.querySelector('[data-testid="diag-vector-tables"]')).toBeNull()
  })
})

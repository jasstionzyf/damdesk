// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, waitFor } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'

/**
 * DT-B / 卡 task：OCR「模型未就绪」的常驻提示 —— **区块已迁入设置页**。
 *
 * 口径（用户裁决：一个能力只归一个区域）：
 *  - 完整 OCR 区块（统计 / 缺件清单 / 识别动作）在设置页「文字识别（OCR）」分区，
 *    testid 沿用 `ocr-section` / `ocr-models-missing`；
 *  - 打标任务页只留一行可点提示（`ocr-not-ready-hint`），点击落到设置页该分区
 *    （`/settings#ocr` → 该分区 `data-landed="true"`），不再在任务页重复整套控件。
 *
 * 未就绪信息来自后端常驻的 /api/ocr/status（modelsReady / missingModelFiles 绝对路径），
 * 不是「点了才吃 409」。
 */

const { apiMock, ocrState, OCR_MISSING } = vi.hoisted(() => {
  const OCR_MISSING = [
    'C:\\Users\\me\\AppData\\Roaming\\com.soujpg.dam.desktop\\models\\ppocrv6-medium-det\\inference.onnx',
    'C:\\Users\\me\\AppData\\Roaming\\com.soujpg.dam.desktop\\models\\ppocrv6-medium-rec\\inference.onnx',
    'C:\\Users\\me\\AppData\\Roaming\\com.soujpg.dam.desktop\\models\\ppocrv6-medium-rec\\ppocrv6_dict.txt',
  ]
  const ocrState = { ready: false, missing: [...OCR_MISSING] as string[] }
  return {
    OCR_MISSING,
    ocrState,
    apiMock: {
      tagStatus: vi.fn(async () => ({
        template: { running: false, progress: null, report: null, lastError: null },
        custom: { running: false, progress: null, report: null, lastError: null, pendingJobs: 0 },
        review: { assets: 0, rows: 0 },
        stats: { done: 0, pending: 0, failed: 0, untaggedAssets: 0 },
        provider: {
          enabled: false,
          configured: true,
          hasKey: false,
          provider: 'damai',
          baseUrl: 'http://host.docker.internal:8091/v1',
          model: 'qwen3.5-4b',
          activeModel: 'damai:qwen3.5-4b',
          maxTokens: 2048,
          maxTokensSource: 'default',
          maxTokensLabel: '2048（默认）',
          defaultMaxTokens: 2048,
        },
        models: [],
      })),
      tagTasks: vi.fn(async () => ({ items: [] })),
      tagFailed: vi.fn(async () => ({ items: [] })),
      tagCustomFailed: vi.fn(async () => ({ items: [] })),
      ocrStatus: vi.fn(async () => ({
        running: false,
        stats: { done: 0, failed: 0, pending: 0, noOcrAssets: 0 },
        progress: null,
        report: null,
        error: null,
        modelsReady: ocrState.ready,
        missingModelFiles: ocrState.ready ? [] : ocrState.missing,
      })),
      ocrStart: vi.fn(async () => ({ enqueued: 0, mode: 'pending', limit: 0 })),
      ocrCancel: vi.fn(async () => ({ cancelling: true })),
      ocrFailed: vi.fn(async () => ({ items: [] })),
      ocrRetryFailed: vi.fn(async () => ({ requeued: 0 })),
      // 设置页其余分区（本用例只需它们不炸）
      inference: vi.fn(async () => ({
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
        embedders: [],
        imageOnlyEmbedders: [],
        capabilities: { localInfer: false, remoteProvider: true, note: '' },
      })),
      saveInference: vi.fn(),
      testEmbed: vi.fn(),
      embedStatus: vi.fn(async () => null),
      embedFailed: vi.fn(async () => ({ items: [] })),
      embedStart: vi.fn(),
      embedCancel: vi.fn(),
      embedRetryFailed: vi.fn(),
      tagPresets: vi.fn(async () => ({ presets: [] })),
      tagTest: vi.fn(),
      tagScan: vi.fn(),
      exifSettings: vi.fn(async () => null),
      exifSettingsSave: vi.fn(),
      exifStatus: vi.fn(async () => null),
      exifBackfill: vi.fn(),
      // 卡 task：设置页新增「模型文件（可选下载）」区块（本用例只关心 OCR 未就绪提示）
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
      searchStatus: vi.fn(async () => null),
      stats: vi.fn(async () => null),
      appInfo: vi.fn(async () => null),
    },
  }
})

vi.mock('../src/lib/api', () => ({
  api: apiMock,
  DEFAULT_TAG_MAX_TOKENS: 2048,
  formatSize: (n: number) => `${n} B`,
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

import { TagTasks } from '../src/pages/TagTasks'
import { Settings } from '../src/pages/Settings'

beforeEach(() => {
  vi.clearAllMocks()
  ocrState.ready = false
  ocrState.missing = [...OCR_MISSING]
})

afterEach(() => cleanup())

describe('OCR 未就绪提示：区块在设置页，任务页只留一行入口（task）', () => {
  it('设置页 modelsReady=false：黄条列出缺失绝对路径 + 来源与「无需重启」', async () => {
    render(
      <MemoryRouter initialEntries={['/settings']}>
        <Settings />
      </MemoryRouter>,
    )
    const box = (await waitFor(() => {
      const el = document.querySelector('[data-testid="ocr-section"] [data-testid="ocr-models-missing"]')
      if (!el) throw new Error('设置页未就绪提示没渲染')
      return el
    })) as HTMLElement

    expect(box.textContent).toContain('OCR 模型未就绪')
    for (const p of OCR_MISSING) expect(box.textContent).toContain(p)
    expect(box.textContent).toContain('Apache-2.0')
    expect(box.textContent).toContain('无需重启')
    // 未就绪时不能点「识别」（点了必失败）——入口灰置，不留假可点
    expect(document.querySelector('[data-testid="ocr-start-pending"]')?.hasAttribute('disabled')).toBe(true)
  })

  it('设置页 modelsReady=true：不显示黄条，识别入口可用', async () => {
    ocrState.ready = true
    ocrState.missing = []
    render(
      <MemoryRouter initialEntries={['/settings']}>
        <Settings />
      </MemoryRouter>,
    )
    await waitFor(() => expect(apiMock.ocrStatus).toHaveBeenCalled())
    await waitFor(() => expect(document.querySelector('[data-testid="ocr-readiness"]')?.textContent).toContain('就绪'))
    expect(document.querySelector('[data-testid="ocr-models-missing"]')).toBeNull()
    expect(document.querySelector('[data-testid="ocr-start-pending"]')?.hasAttribute('disabled')).toBe(false)
  })

  it('任务页只留一行可点提示（unready 才出现，ready 不出现）', async () => {
    const { unmount } = render(
      <MemoryRouter initialEntries={['/tag-tasks']}>
        <TagTasks />
      </MemoryRouter>,
    )
    const hint = await waitFor(() => {
      const el = document.querySelector('[data-testid="ocr-not-ready-hint"]')
      if (!el) throw new Error('任务页未就绪提示没渲染')
      return el as HTMLAnchorElement
    })
    expect(hint.getAttribute('href')).toBe('/settings#ocr')
    // 任务页不再承载整套 OCR 控件（一能力一处）
    expect(document.querySelector('[data-testid="ocr-section"]')).toBeNull()
    unmount()

    ocrState.ready = true
    ocrState.missing = []
    render(
      <MemoryRouter initialEntries={['/tag-tasks']}>
        <TagTasks />
      </MemoryRouter>,
    )
    await waitFor(() => expect(apiMock.ocrStatus).toHaveBeenCalled())
    expect(document.querySelector('[data-testid="ocr-not-ready-hint"]')).toBeNull()
  })

  it('点任务页提示 → 落到设置页 OCR 分区（data-landed=true，一眼看见）', async () => {
    render(
      <MemoryRouter initialEntries={['/tag-tasks']}>
        <Routes>
          <Route path="/tag-tasks" element={<TagTasks />} />
          <Route path="/settings" element={<Settings />} />
        </Routes>
      </MemoryRouter>,
    )
    const hint = await waitFor(() => {
      const el = document.querySelector('[data-testid="ocr-not-ready-hint"]')
      if (!el) throw new Error('任务页未就绪提示没渲染')
      return el as HTMLElement
    })
    fireEvent.click(hint)

    const landed = await waitFor(() => {
      const el = document.querySelector('[data-testid="ocr-section"]')
      if (!el || el.getAttribute('data-landed') !== 'true') throw new Error('没落到 OCR 分区')
      return el as HTMLElement
    })
    expect(landed.id).toBe('ocr')
    // 落地后仍能看到缺件清单（用户是带着问题来的，别让他再找一次）
    await waitFor(() =>
      expect(document.querySelector('[data-testid="ocr-models-missing"]')).not.toBeNull(),
    )
  })

  it('未就绪但后端没给缺件路径：文案不出现「缺 0 件」，改为指向模型目录', async () => {
    ocrState.missing = [] // 模型根未配置 → 后端 modelsReady=false 且 missingModelFiles=[]
    render(
      <MemoryRouter initialEntries={['/settings']}>
        <Settings />
      </MemoryRouter>,
    )
    const line = await waitFor(() => {
      const el = document.querySelector('[data-testid=ocr-readiness]')
      if (!el || !el.textContent?.includes('未就绪')) throw new Error('没显示未就绪')
      return el as HTMLElement
    })
    expect(line.textContent).not.toContain('缺 0 件')
    expect(document.querySelector('[data-testid=ocr-models-missing]')?.textContent).toContain('模型根可能未配置')
  })

  it('老后端没有 /api/ocr/status：如实报「读取失败」，不挂「读取中…」装加载', async () => {
    // 真机上常见：装的是旧版 app-web（路由不存在 → 返回 index.html → JSON 解析失败）
    apiMock.ocrStatus.mockRejectedValueOnce(new SyntaxError('Unexpected token < in JSON'))
    render(
      <MemoryRouter initialEntries={['/settings']}>
        <Settings />
      </MemoryRouter>,
    )
    const line = await waitFor(() => {
      const el = document.querySelector('[data-testid="ocr-readiness"]')
      if (!el || !el.textContent?.includes('读取失败')) throw new Error('没报读取失败')
      return el as HTMLElement
    })
    expect(line.textContent).toContain('Unexpected token')
    expect(line.textContent).not.toContain('读取中')
  })
})

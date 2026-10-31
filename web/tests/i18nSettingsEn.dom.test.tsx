// @vitest-environment jsdom
/**
 * i18n-D 验收（卡 task）：en-US 下渲染设置页，DOM 无 CJK。
 *
 * 口径：
 *  - locale = en-US（localStorage `dam.locale`，LocaleProvider mount 时读它并同步模块级
 *    globalLocale —— t() 与 tGlobal 两条通道都是 en）；
 *  - 断言 document.body.textContent 里 [\u4e00-\u9fff] 计数 0，例外白名单 ≤7 条硬编码在下方；
 *  - 白名单现状（4 条，均为设计使然或 C 卡范围组件，不属 D 卡设置页文案）：
 *      1. '语言'    —— 语言分区标题有意硬编码双语「语言 / Language」（i18n-A：语言选择器
 *                     本身是翻译的唯一例外，W3C 本地化惯例）；
 *      2. '简体中文' —— 下拉里的 locale 自展示名（i18n-A 设计：始终本地语言书写，不翻译）；
 *      3. '密度'    —— components/DensityControl.tsx（C 卡批量迁移范围）的可见文案；
 *      4. '列'      —— 同上，密度读数「170px · N 列」。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

const { apiMock } = vi.hoisted(() => {
  const LAMA_PATH = '/home/u/.local/share/com.soujpg.dam.desktop/models/lama-fp32/lama-fp32.onnx'
  const apiMock = {
    models: vi.fn(async () => ({
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
          ready: false,
          presentBytes: 0,
          totalBytes: 208_044_816,
          missingPaths: [LAMA_PATH],
          files: [],
        },
      ],
    })),
    modelDownload: vi.fn(async (name: string) => ({ ok: true, model: name, baseUrl: 'u', root: 'r', totalBytes: 1, files: 1 })),
    modelDownloadStatus: vi.fn(async () => ({
      running: false,
      model: null,
      status: null,
      doneBytes: 0,
      totalBytes: 208_044_816,
      completedFiles: 0,
      totalFiles: 1,
      error: null,
      outcome: null,
      startedAt: 1,
      finishedAt: 2,
    })),
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
  return { apiMock }
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

import { LOCALE_STORAGE_KEY, LocaleProvider } from '../src/i18n'
import { Settings } from '../src/pages/Settings'

/** 允许残留的 CJK（≤7，超出即 FAIL） */
const CJK_WHITELIST = ['密度', '列', '简体中文', '语言']

beforeEach(() => {
  vi.clearAllMocks()
  window.localStorage.clear()
  window.localStorage.setItem(LOCALE_STORAGE_KEY, 'en-US')
})

afterEach(() => {
  cleanup()
  window.localStorage.clear()
})

describe('i18n-D：en-US 下设置页 DOM 无 CJK', () => {
  it('整页（含模型区/诊断/EXIF/provider 表单）无中文残留，且关键词条确实是英文', async () => {
    render(
      <LocaleProvider>
        <MemoryRouter initialEntries={['/settings']}>
          <Settings />
        </MemoryRouter>
      </LocaleProvider>,
    )

    // 等最深的数据块渲染完（模型行 + EXIF 区），避免「扫得太早漏掉异步区」
    await waitFor(() => expect(screen.getByTestId('model-row-lama-fp32')).toBeTruthy())
    await waitFor(() => expect(screen.getByTestId('exif-settings')).toBeTruthy())

    // 关键词条抽查：词典命中（不是中文 key 兜底）
    expect(screen.getByTestId('models-section').textContent).toContain('Model files (optional downloads)')
    expect(screen.getByTestId('model-state-lama-fp32').textContent).toContain('Not downloaded')
    expect(screen.getByTestId('settings-language-note').textContent).toContain('Applies immediately, no restart needed')

    const text = document.body.textContent ?? ''
    const cjkRuns = Array.from(new Set(text.match(/[\u4e00-\u9fff]+/g) ?? []))
    const unexpected = cjkRuns.filter((r) => !CJK_WHITELIST.includes(r))
    expect(
      unexpected,
      `en-US 下设置页出现白名单外的中文：${unexpected.join(' / ')}`,
    ).toEqual([])
    expect(cjkRuns.length).toBeLessThanOrEqual(7)
  })
})

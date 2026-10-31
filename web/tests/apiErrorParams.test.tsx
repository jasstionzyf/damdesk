// @vitest-environment jsdom
/**
 * i18n-E2（卡 task）：后端错误 `{error, code, params}` → 前端展示文案通道。
 *
 * 契约（零破坏）：
 *  - en（词典命中 errors.<code>）→ 英文插值（params 进 {name}）；
 *  - zh（薄壳无词条）→ 后端中文原句（error 字段）原样显示；
 *  - 未收录 code / 无 code → 原句兜底（行为与 E2 前一致）；
 *  - ApiError.params 透传后端 params（仅 string/number）；
 *  - hint/action 语义不动（judge 前置错误仍带去处）。
 */
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { ApiError, apiErrorMessage, api } from '../src/lib/api'
import { LocaleProvider, LOCALE_STORAGE_KEY, useI18n, getGlobalLocale } from '../src/i18n'

function LocaleProbe() {
  const { locale, setLocale } = useI18n()
  return (
    <div>
      <span data-testid="locale">{locale}</span>
      <button data-testid="to-en" onClick={() => setLocale('en-US')}>
        en
      </button>
      <button data-testid="to-zh" onClick={() => setLocale('zh-Hans')}>
        zh
      </button>
    </div>
  )
}

function mountProbe() {
  return render(
    <LocaleProvider>
      <LocaleProbe />
    </LocaleProvider>,
  )
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
  window.localStorage.clear()
})

describe('apiErrorParams：{error, code, params} → 展示文案', () => {
  it('en 命中词典：英文插值（params 进占位符），params 透传到 ApiError', async () => {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, 'en-US')
    const probe = mountProbe()
    expect(getGlobalLocale()).toBe('en-US')

    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        new Response(
          JSON.stringify({
            error: '不是文件夹：/tmp/pics',
            code: 'dedup_dir_not_found',
            params: { path: '/tmp/pics' },
          }),
          { status: 400 },
        ),
      ),
    )
    let caught: unknown
    try {
      await api.searchStatus()
    } catch (e) {
      caught = e
    }
    const err = caught as ApiError
    expect(err).toBeInstanceOf(ApiError)
    expect(err.code).toBe('dedup_dir_not_found')
    expect(err.params).toEqual({ path: '/tmp/pics' })
    // en：词典插值
    expect(err.message).toBe('Directory does not exist or is not a folder: /tmp/pics')
    expect(apiErrorMessage(err)).toBe('Directory does not exist or is not a folder: /tmp/pics')
    probe.unmount()
  })

  it('zh 薄壳：无词条 → 后端中文原句原样（fallback 不动）', async () => {
    Object.defineProperty(window.navigator, 'language', {
      value: 'zh-CN',
      configurable: true,
    })
    const probe = mountProbe()
    expect(getGlobalLocale()).toBe('zh-Hans')

    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        new Response(
          JSON.stringify({
            error: '不是文件夹：/tmp/pics',
            code: 'dedup_dir_not_found',
            params: { path: '/tmp/pics' },
          }),
          { status: 400 },
        ),
      ),
    )
    let caught: unknown
    try {
      await api.searchStatus()
    } catch (e) {
      caught = e
    }
    const err = caught as ApiError
    expect(err.message).toBe('不是文件夹：/tmp/pics')
    expect(apiErrorMessage(err)).toBe('不是文件夹：/tmp/pics')
    probe.unmount()
  })

  it('同错误跨语言切换：en 英文 / zh 回中文原句（运行时切换生效）', () => {
    const probe = mountProbe()
    const err = new ApiError('导入失败：3 张图无法解码', 400, 'import_already_running', undefined, undefined, {
      n: 3,
    })
    expect(err.message).toBe('导入失败：3 张图无法解码')
    fireEvent.click(screen.getByTestId('to-en'))
    expect(apiErrorMessage(err)).toBe('An import batch is already running')
    fireEvent.click(screen.getByTestId('to-zh'))
    expect(apiErrorMessage(err)).toBe('导入失败：3 张图无法解码')
    probe.unmount()
  })

  it('未收录 code / 无 code：原句兜底（E2 前行为不变）', () => {
    const uncoded = new ApiError('读取目录失败（/x）：perm', 500, 'totally_unknown_code')
    expect(apiErrorMessage(uncoded)).toBe('读取目录失败（/x）：perm')
    const noCode = new ApiError('503 Service Unavailable', 503)
    expect(apiErrorMessage(noCode)).toBe('503 Service Unavailable')
    const notApiErr = new Error('plain')
    expect(apiErrorMessage(notApiErr)).toBe('plain')
  })

  it('params 脏值过滤：非 string/number 不进插值，空 params 不产生词条命中误判', async () => {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, 'en-US')
    const probe = mountProbe()
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        new Response(
          JSON.stringify({
            error: '导入失败：3 张图无法解码',
            code: 'dedup_ids_empty',
            params: { nested: { a: 1 }, ok: 3, when: true },
          }),
          { status: 400 },
        ),
      ),
    )
    let caught: unknown
    try {
      await api.searchStatus()
    } catch (e) {
      caught = e
    }
    const err = caught as ApiError
    expect(err.params).toEqual({ ok: 3 })
    expect(err.message).toBe('ids must not be empty')
    probe.unmount()
  })

  it('judge 前置错误（hint+message 老形状）：仍显示 message 人话，hint/action 语义不动', async () => {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, 'en-US')
    const probe = mountProbe()
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        new Response(
          JSON.stringify({
            error: 'plugin_not_installed',
            message: '没装处理插件（com.soujpg.pack.foo）：判定由插件执行，没有插件就没有判定',
            code: 'plugin_not_installed',
            params: { pluginId: 'com.soujpg.pack.foo' },
            hint: '点「去安装插件」装 .dampack 后再跑',
            action: 'open_packs_drawer',
          }),
          { status: 400 },
        ),
      ),
    )
    let caught: unknown
    try {
      await api.searchStatus()
    } catch (e) {
      caught = e
    }
    const err = caught as ApiError
    expect(err.hint).toBe('点「去安装插件」装 .dampack 后再跑')
    expect(err.action).toBe('open_packs_drawer')
    // en 命中主文案；hint 本轮保留后端原文（E2 边界：hint 不翻译）
    expect(err.message).toBe(
      'Processing plugin not installed (com.soujpg.pack.foo): judgement runs inside the plugin, no plugin means no judgement',
    )
    probe.unmount()
  })
})

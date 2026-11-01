/**
 * i18n-D（卡 t_4eea3e75）：ApiError 错误码 i18n 兜底 + formatDateTime locale 口径。
 *
 * 钉住的行为：
 *  1) apiErrorMessage：en-US 且 dict 有 `apiError.<code>` 词条 → 译文；zh-Hans / 未知码 /
 *     无码 → 后端原文**原样透出**（缺词条不顶 key，行为与迁移前一致）；
 *  2) jfetch 端到端：非 2xx 带 code 的响应，抛出的 ApiError.message 是译文、code 保留；
 *  3) formatDateTime：zh `MM-DD HH:mm`（同年省年、跨年补年），en `MMM D, YYYY, h:mm A`；
 *  4) fmtJobDuration：zh `X分Y秒`，en `Xm Ys`。
 */
import { afterEach, describe, expect, it, vi } from 'vitest'

import { formatDateTime, fmtJobDuration } from '../src/lib/jobs'
import { ApiError, apiErrorMessage } from '../src/lib/api'

afterEach(() => {
  vi.unstubAllGlobals()
  vi.resetModules()
})

/** 重置模块图后按指定 locale 重新 import（模块级 globalLocale 在 import 时判定） */
async function importFresh(locale: 'zh-Hans' | 'en-US') {
  vi.resetModules()
  ;(globalThis as unknown as { localStorage: Storage }).localStorage = {
    getItem: (k: string) => (k === 'dam.locale' ? locale : null),
    setItem: () => {},
    removeItem: () => {},
    clear: () => {},
  } as unknown as Storage
  return await import('../src/lib/api')
}

describe('apiErrorMessage（错误码 → locale 译文，缺词条原样透出）', () => {
  it('zh-Hans（测试默认）：有码有词条也不翻 —— 后端中文人话就是目标语言', () => {
    // i18n-G 收敛后单通道签名 = apiErrorMessage(e)：zh 薄壳不收 errors.* ⇒ 恒回落后端原句
    expect(apiErrorMessage(new ApiError('没有可用的向量索引', 500, 'no_vector_index'))).toBe(
      '没有可用的向量索引',
    )
  })

  it('en-US：已知码 → dict 译文（`errors.<code>` 通道 + params 插值）', async () => {
    const mod = await importFresh('en-US')
    expect(
      mod.apiErrorMessage(
        new mod.ApiError('没有可用的向量索引', 500, 'no_vector_index', undefined, undefined, {
          model: 'siglip2',
        }),
      ),
    ).toBe(
      'The current model has no vector index yet (siglip2) — run an embedding pass first (Gallery or Settings)',
    )
  })

  it('en-US：未知码 / 无码 / 非 ApiError → 原文透出（不顶 key）', async () => {
    const mod = await importFresh('en-US')
    expect(mod.apiErrorMessage(new mod.ApiError('模型 x 不存在', 500, 'some_new_code'))).toBe(
      '模型 x 不存在',
    )
    expect(mod.apiErrorMessage(new mod.ApiError('plain failure', 500))).toBe('plain failure')
    expect(mod.apiErrorMessage('not an error object')).toBe('not an error object')
  })

  it('jfetch 端到端（en）：非 2xx + code + params → ApiError.message 是插值译文、code 原样保留', async () => {
    const mod = await importFresh('en-US')
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        new Response(
          JSON.stringify({
            error: '没有可用的向量索引',
            code: 'no_vector_index',
            params: { model: 'siglip2' },
          }),
          { status: 500, headers: { 'content-type': 'application/json' } },
        ),
      ),
    )
    const err = await mod.api.models().catch((e: unknown) => e)
    expect(err).toBeInstanceOf(mod.ApiError)
    const apiErr = err as InstanceType<typeof mod.ApiError>
    expect(apiErr.code).toBe('no_vector_index')
    expect(apiErr.message).toBe(
      'The current model has no vector index yet (siglip2) — run an embedding pass first (Gallery or Settings)',
    )
  })
})

describe('formatDateTime / fmtJobDuration（locale 口径）', () => {
  it('zh：同年省年份，跨年补年份', () => {
    const nowYear = new Date().getFullYear()
    const sameYear = new Date(nowYear, 9, 25, 14, 3).getTime()
    expect(formatDateTime(sameYear, 'zh-Hans')).toBe('10-25 14:03')
    const lastYear = new Date(nowYear - 1, 9, 25, 14, 3).getTime()
    expect(formatDateTime(lastYear, 'zh-Hans')).toBe(`${nowYear - 1}-10-25 14:03`)
  })

  it('en：`MMM D, YYYY, h:mm A`（12 小时制 + AM/PM，0 点归 12）', () => {
    expect(formatDateTime(new Date(2026, 9, 25, 14, 3).getTime(), 'en-US')).toBe('Oct 25, 2026, 2:03 PM')
    expect(formatDateTime(new Date(2026, 0, 2, 0, 5).getTime(), 'en-US')).toBe('Jan 2, 2026, 12:05 AM')
  })

  it('fmtJobDuration：zh `X分Y秒` / en `Xm Ys`（≥1 分钟段才分化）', async () => {
    const t0 = 1_000_000
    expect(fmtJobDuration(t0, t0 + 65_000)).toBe('1分5秒')
    await importFresh('en-US')
    const { fmtJobDuration: enDur } = await import('../src/lib/jobs')
    expect(enDur(t0, t0 + 65_000)).toBe('1m 5s')
    // 秒级两 locale 同形
    expect(fmtJobDuration(t0, t0 + 1_500, 1_500)).toBe('1.5s')
  })
})

/**
 * CMP-JOB 跨层 E2E 的「前端 ↔ 后端」接缝单测（卡 task）。
 *
 * 父卡 task 的 13 条 jsdom 断言全都 mock 掉了 `api`，所以**真正发请求的那一层**
 * （`api.complianceJobs` / `complianceJob` / `complianceJobCurrent` 拼的 URL、错误如何冒出来）
 * 一行都没被测过。这里用真 `api` 模块 + fetch 桩守住接缝，并把「计数口径」的恒等式
 * 写成可执行断言 —— 口径错了（跳过并进失败 / 取消并进已处理）这里先红，而不是等界面走查。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ApiError, api } from '../src/lib/api'
import {
  cancelledNote,
  countsShort,
  countsText,
  jobCounts,
  jobHeadline,
  processed,
  progressPct,
  queuedNote,
  taskReason,
  taskReasonLine,
  taskResult,
} from '../src/lib/complianceJobs'
import type { ComplianceJobTaskView, ComplianceJobView } from '../src/lib/api'

function job(over: Partial<ComplianceJobView> = {}): ComplianceJobView {
  return {
    runId: 7,
    status: 'done',
    statusLabel: '已完成',
    startedAt: 1_760_000_000,
    finishedAt: 1_760_000_018,
    total: 10,
    done: 7,
    failed: 1,
    skipped: 2,
    cancelled: 0,
    durationMs: 18_000,
    error: null,
    params: null,
    ...over,
  }
}

function task(over: Partial<ComplianceJobTaskView> = {}): ComplianceJobTaskView {
  return {
    jobId: 501,
    targetId: 1,
    step: '逐图判定 · 规则 F0_domain',
    assetId: 1234,
    status: 'done',
    statusLabel: '已完成',
    attempts: 1,
    lastError: null,
    updatedAt: 1_760_000_010,
    ...over,
  }
}

const calls: { url: string; method: string }[] = []

function stubFetch(body: unknown, status = 200) {
  const f = vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(url), method: (init?.method || 'GET').toUpperCase() })
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    })
  })
  vi.stubGlobal('fetch', f)
  return f
}

beforeEach(() => {
  calls.length = 0
})
afterEach(() => {
  vi.unstubAllGlobals()
})

// ---------------------------------------------------------------- HTTP 契约
describe('执行列表 API 契约（真 api 模块，只桩 fetch）', () => {
  it('complianceJobs() 打的是 /api/compliance/jobs，无参数时不带空 query', async () => {
    stubFetch({ total: 0, limit: 20, offset: 0, jobs: [] })
    const r = await api.complianceJobs()
    expect(calls[0]).toEqual({ url: '/api/compliance/jobs', method: 'GET' })
    expect(r.total).toBe(0)
  })

  it('分页参数真的拼进 query（列表翻页靠它）', async () => {
    stubFetch({ total: 6, limit: 2, offset: 4, jobs: [] })
    await api.complianceJobs({ limit: 2, offset: 4 })
    expect(calls[0].url).toBe('/api/compliance/jobs?limit=2&offset=4')
  })

  it('单次明细：runId 进路径，offset=100 进 query（明细一页 100 条）', async () => {
    stubFetch({ run: job(), taskTotal: 0, limit: 100, offset: 100, tasks: [] })
    await api.complianceJob(7, { limit: 100, offset: 100 })
    expect(calls[0].url).toBe('/api/compliance/jobs/7?limit=100&offset=100')
  })

  it('「正在跑的那次」有独立端点（界面据此收起/显示置顶进度卡）', async () => {
    stubFetch({ run: null })
    const r = await api.complianceJobCurrent()
    expect(calls[0].url).toBe('/api/compliance/jobs/current')
    expect(r.run).toBeNull()
  })

  it('服务端 5xx 要响亮抛出（不是返回空列表装作「没跑过」）', async () => {
    stubFetch({ error: 'jobs', message: 'database is locked' }, 500)
    await expect(api.complianceJobs()).rejects.toBeInstanceOf(ApiError)
    await expect(api.complianceJobs()).rejects.toThrow(/jobs/)
  })
})

// ---------------------------------------------------------------- 计数口径
describe('计数口径（跳过不是失败，取消不是「已处理」）', () => {
  it('countsText 的四个数字必须与后端字段一一对应，且满足 total=done+failed+skipped+cancelled', () => {
    const j = job()
    expect(countsText(j)).toBe('成功 7 / 失败 1 / 跳过 2 / 共 10')
    const c = jobCounts(j)
    expect(c.done + c.failed + c.skipped + c.cancelled).toBe(c.total)
  })

  it('跳过不并进成功也不并进失败（前端只读后端，不做口径修正）', () => {
    const j = job({ done: 8, failed: 0, skipped: 2, total: 10 })
    const c = jobCounts(j)
    expect(c.done).toBe(8)
    expect(c.skipped).toBe(2)
    expect(c.failed).toBe(0)
    expect(countsShort(j)).toBe('成 8 · 败 0 · 跳 2')
  })

  it('进度分子 = 成功+失败+跳过；取消后未执行的不算处理过（进度条不许假满）', () => {
    const cancelledRun = job({ total: 10, done: 0, failed: 0, skipped: 3, cancelled: 7, finishedAt: null, status: 'cancelled' })
    expect(processed(cancelledRun)).toBe(3)
    expect(progressPct(cancelledRun)).toBe(30)
  })

  it('total=0 时进度是 0，不是 NaN/100', () => {
    const empty = job({ total: 0, done: 0, failed: 0, skipped: 0, cancelled: 0 })
    expect(progressPct(empty)).toBe(0)
  })

  it('徽章：「已完成（有失败）」只在真失败时出现；取消不被写成有失败', () => {
    expect(jobHeadline(job({ done: 7, failed: 1 }))).toBe('已完成（有失败）')
    expect(jobHeadline(job({ done: 8, failed: 0, skipped: 2 }))).toBe('已完成')
    expect(jobHeadline(job({ status: 'cancelled', statusLabel: '已取消', cancelled: 4, done: 6, failed: 0 }))).toBe('已取消')
  })
})

// ---------------------------------------------------------------- 运行中口径（GAP-2 / 卡 task）
describe('运行中口径：cancelled 是「未执行」，排队走 queued', () => {
  function running(over: Partial<ComplianceJobView> = {}) {
    return job({
      status: 'running',
      statusLabel: '进行中',
      finishedAt: null,
      total: 10,
      done: 2,
      failed: 0,
      skipped: 2,
      cancelled: 0,
      queued: 6,
      ...over,
    })
  }

  it('运行中行不出现「未执行」：后端 cancelled 恒 0，未结算条数落在 queued', () => {
    const j = running()
    expect(countsText(j)).toBe('成功 2 / 失败 0 / 跳过 2 / 共 10')
    expect(countsText(j)).not.toContain('未执行')
    expect(cancelledNote(j)).toBeNull()
    expect(queuedNote(j)).toBe('排队 6')
    expect(jobCounts(j).queued).toBe(6)
  })

  it('运行中恒等式：total = done+failed+skipped+queued（进度条分母不虚）', () => {
    const c = jobCounts(running())
    expect(c.done + c.failed + c.skipped + c.queued).toBe(c.total)
    expect(processed(running())).toBe(4)
    expect(progressPct(running())).toBe(40)
  })

  it('老后端把 pending 塞进 cancelled 时，运行中也不显示「未执行」（双保险）', () => {
    const legacy = running({ cancelled: 6, queued: undefined })
    expect(cancelledNote(legacy)).toBeNull()
    expect(countsText(legacy)).toBe('成功 2 / 失败 0 / 跳过 2 / 共 10')
    expect(jobCounts(legacy).queued).toBe(0) // 缺字段按 0
  })

  it('收工后反过来：queued 归零，cancelled 才是「未执行」', () => {
    const fin = job({ status: 'cancelled', statusLabel: '已取消', total: 10, done: 0, failed: 0, skipped: 3, cancelled: 7, queued: 0 })
    expect(queuedNote(fin)).toBeNull()
    expect(cancelledNote(fin)).toBe('未执行 7')
    expect(countsText(fin)).toBe('成功 0 / 失败 0 / 跳过 3 / 共 10 / 未执行 7')
  })

  it('/jobs/current 的 queue 给了 queued/cancelled：排队≠未执行（老后端缺字段按 0）', async () => {
    stubFetch({
      run: running(),
      queue: {
        runId: 7,
        assetDone: 2,
        assetFailed: 0,
        assetPending: 3,
        groupDone: 0,
        groupFailed: 0,
        groupPending: 3,
        skipped: 2,
        queued: 6,
        cancelled: 0,
      },
    })
    const r = await api.complianceJobCurrent()
    expect(r.queue?.queued).toBe(6)
    expect(r.queue?.cancelled).toBe(0)
    expect(r.run && countsText(r.run)).not.toContain('未执行')
    stubFetch({ run: running(), queue: { runId: 7, assetDone: 0, assetFailed: 0, assetPending: 0, groupDone: 0, groupFailed: 0, groupPending: 0, skipped: 0 } })
    const legacy = await api.complianceJobCurrent()
    expect(legacy.queue?.queued ?? 0).toBe(0)
    expect(legacy.queue?.cancelled ?? 0).toBe(0)
  })
})

describe('task 三态（跳过与成功同终态，只能靠原因前缀区分）', () => {
  it('done + not_image → 跳过（不是成功、不是失败）', () => {
    const t = task({ status: 'done', lastError: 'not_image: 头部字节不是已知图像签名' })
    const r = taskResult(t)
    expect(r.word).toBe('skip')
    expect(r.label).toBe('跳过')
    expect(r.detail).toContain('not_image:')
    expect(taskReason(t.lastError)?.kind).toBe('skip')
  })

  it('done 且没有原因 → 已完成', () => {
    expect(taskResult(task()).word).toBe('ok')
  })

  it('failed 优先于原因前缀：带 not_image 前缀的 failed 仍算失败（状态为准，前缀只作解释）', () => {
    const t = task({ status: 'failed', lastError: 'not_image: x' })
    expect(taskResult(t).word).toBe('fail')
  })

  it('unreadable（权限等环境问题）不冒充跳过原因', () => {
    const r = taskReason('unreadable: Permission denied')
    expect(r?.kind).toBe('fail')
    expect(taskResult(task({ status: 'failed', lastError: 'unreadable: Permission denied' })).word).toBe('fail')
  })

  it('file_missing 前缀翻成人话（失联可见，不静默）', () => {
    const r = taskReason('file_missing: No such file or directory')
    expect(r?.kind).toBe('skip')
    expect(r?.text).toContain('文件已失联')
  })

  // CMP-JOB-FIX-1：资产状态分支（已删 / 失联 / 非图片媒体类型）也必须带原因，
  // 否则「run 说跳过 3、明细只有 2 条有原因」，用户点开抽屉无法解释。
  it('deleted 前缀翻成人话（资产已删除 = 跳过，不是失败）', () => {
    const r = taskReason('deleted: 资产已删除')
    expect(r?.kind).toBe('skip')
    expect(r?.text).toContain('已删除')
    const t = task({ status: 'done', lastError: 'deleted: 资产已删除' })
    expect(taskResult(t).word).toBe('skip')
    expect(taskReasonLine(t)?.kind).toBe('skip')
  })

  it('not_image_media 前缀翻成人话，且不被 not_image 抢走', () => {
    const r = taskReason('not_image_media: 媒体类型 video')
    expect(r?.kind).toBe('skip')
    expect(r?.text).toContain('媒体类型')
    expect(taskResult(task({ status: 'done', lastError: 'not_image_media: 媒体类型 video' })).word).toBe('skip')
  })

  it('done + 上一轮残留的失败原文 → 不渲染原因行（不能出现「已完成 + 失败原因」）', () => {
    const t = task({ status: 'done', lastError: 'compliance: rule R0_product_main vl: io: No such file' })
    expect(taskResult(t).word).toBe('ok')
    expect(taskReasonLine(t)).toBeNull()
    // 同一段原文在 failed 行上照常显示（证据不丢）
    expect(taskResult(task({ status: 'failed', lastError: 'compliance: rule R0 vl: io: boom' })).word).toBe('fail')
  })
})

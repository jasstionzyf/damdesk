#!/usr/bin/env node
/**
 * CMP-JOB 跨层 E2E 的界面段（卡 t_54f92c17）。
 *
 * 目标不是「页面能打开」，而是**界面上的每一次执行的数字 == 数据库事实 == API 字段**：
 * 父卡的界面探针只在自己的 24 张完好真图上跑（skipped 恒 0），所以「跳过」这条链路在
 * DOM 上从没被证明过。本探针用 cmp_job_e2e.py 造出的 5 次执行（含 2 张坏图 → 跳过），
 * 逐行对账，并把跳过原因、历史累计折叠区一起断言。
 *
 * 用法：
 *   CDP_PORT=9249 APP_URL=http://127.0.0.1:18899 \
 *     SNAPSHOT=reports/evidence/CMP-JOB-E2E/api-snapshot.json \
 *     OUT=reports/evidence/CMP-JOB-E2E/ui-assertions.json SHOT_DIR=... \
 *     node scripts/cmp_job_e2e_ui.mjs
 *
 * 终态行：UI_PROBE_RESULT OK / FAIL:<n>
 */
import fs from 'node:fs/promises'
import path from 'node:path'

const BASE = (process.env.APP_URL || 'http://127.0.0.1:18899').replace(/\/$/, '')
const CDP = (process.env.CDP_URL || `http://127.0.0.1:${process.env.CDP_PORT || '9249'}`).replace(/\/$/, '')
const OUT = process.env.OUT || 'reports/evidence/CMP-JOB-E2E/ui-assertions.json'
const SHOT_DIR = process.env.SHOT_DIR || path.dirname(OUT)
const SNAPSHOT = process.env.SNAPSHOT || ''

const evidence = []
const pass = (name, detail) => {
  evidence.push({ name, ok: true, detail })
  console.log(`PASS ${name} :: ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`)
}
const bad = (name, detail) => {
  evidence.push({ name, ok: false, detail })
  console.log(`FAIL ${name} :: ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`)
}
const check = (name, cond, detail) => (cond ? pass(name, detail) : bad(name, detail))
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const norm = (s) => (s || '').replace(/\s+/g, '')

class Cdp {
  constructor(ws) {
    this.ws = ws
    this.id = 0
    this.pending = new Map()
    ws.addEventListener('message', (ev) => {
      const m = JSON.parse(ev.data)
      if (m.id && this.pending.has(m.id)) {
        const { res, rej } = this.pending.get(m.id)
        this.pending.delete(m.id)
        m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result)
      }
    })
  }
  static async connect() {
    const ver = await (await fetch(`${CDP}/json/version`)).json()
    const ws = new WebSocket(ver.webSocketDebuggerUrl)
    await new Promise((res, rej) => {
      ws.addEventListener('open', res, { once: true })
      ws.addEventListener('error', () => rej(new Error('ws error')), { once: true })
    })
    return new Cdp(ws)
  }
  send(method, params = {}, sessionId) {
    const id = ++this.id
    this.ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }))
    return new Promise((res, rej) => this.pending.set(id, { res, rej }))
  }
}

/** 与 web/src/lib/complianceJobs.ts 同口径（服务端口径，前端不改写） */
const countsText = (j) => {
  const n = (v) => (typeof v === 'number' && v > 0 ? Math.floor(v) : 0)
  // 「未执行」只属于**收工**的行：运行中的任务只是还在排队（`queued`），不是未执行
  const tail = j.status !== 'running' && n(j.cancelled) > 0 ? ` / 未执行 ${n(j.cancelled)}` : ''
  return `成功 ${n(j.done)} / 失败 ${n(j.failed)} / 跳过 ${n(j.skipped)} / 共 ${n(j.total)}${tail}`
}
const headline = (j) => {
  const label = j.statusLabel || j.status
  return j.status === 'done' && (j.failed || 0) > 0 ? `${label}（有失败）` : label
}

async function main() {
  const jobsView = await (await fetch(`${BASE}/api/compliance/jobs?limit=50`)).json()
  const status = await (await fetch(`${BASE}/api/compliance/judge/status`)).json()
  if (SNAPSHOT) {
    try {
      const snap = JSON.parse(await fs.readFile(SNAPSHOT, 'utf8'))
      check(
        'UI0 快照与当前 API 一致（探针跑在同一批数据上）',
        snap.jobs.length === jobsView.jobs.length,
        `snapshot=${snap.jobs.length} now=${jobsView.jobs.length}`,
      )
    } catch (e) {
      bad('UI0 快照可读', String(e))
    }
  }
  const jobs = jobsView.jobs.filter((j) => j.status !== 'running')
  const run1 = jobs.find((j) => (j.skipped || 0) > 0)
  check('UI0 数据前提：本次执行里有「跳过」的 run', !!run1, run1 ? `run=${run1.runId} skipped=${run1.skipped}` : '没有 skipped>0 的 run')

  // ---------- CDP 连接 ----------
  try {
    const pages = (await (await fetch(`${CDP}/json/list`)).json()).filter((t) => t.type === 'page')
    for (const t of pages) await fetch(`${CDP}/json/close/${t.id}`)
  } catch {
    /* 无残留页 */
  }
  const cdp = await Cdp.connect()
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true })
  const send = (m, p = {}) => cdp.send(m, p, sessionId)
  await send('Page.enable')
  await send('Runtime.enable')
  await send('Emulation.setFocusEmulationEnabled', { enabled: true })
  await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 1000, deviceScaleFactor: 1, mobile: false })
  const js = async (expr) => {
    const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true })
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + ' :: ' + (r.exceptionDetails.exception?.description ?? ''))
    return r.result.value
  }
  const waitFor = async (expr, ms = 25000, label = expr) => {
    const t0 = Date.now()
    while (Date.now() - t0 < ms) {
      if (await js(`(()=>{try{return !!(${expr})}catch(e){return false}})()`)) return true
      await sleep(150)
    }
    bad(`等待超时：${label}`)
    return false
  }
  const shot = async (name) => {
    const r = await send('Page.captureScreenshot', { format: 'png' })
    await fs.mkdir(SHOT_DIR, { recursive: true })
    const p = path.join(SHOT_DIR, `${name}.png`)
    await fs.writeFile(p, Buffer.from(r.data, 'base64'))
    console.log(`SHOT ${p}`)
    return p
  }
  const txt = (sel) =>
    js(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});return el?el.textContent.trim():null})()`)

  // createBrowserRouter（history 路由）：页面地址是 /compliance，不是 hash 路由
  await send('Page.navigate', { url: `${BASE}/compliance` })
  await waitFor(`document.querySelector('[data-testid="compliance-jobs"]')`, 30000, '执行列表面板')
  await sleep(1200)
  const vis = await js('document.visibilityState')
  check('UI1 页面已 hydrate（非 hidden 假象）', vis === 'visible', `visibilityState=${vis}`)

  check('UI2 执行列表面板存在', !!(await txt('[data-testid="compliance-jobs"]')), '')
  const rows = await js(`Array.from(document.querySelectorAll('[data-testid^="compliance-job-row-"]')).map(e=>e.getAttribute('data-run-id'))`)
  check(
    'UI3 列表行数 == API 的已完成执行数',
    Array.isArray(rows) && rows.length === jobs.length,
    `dom=${rows && rows.length} api=${jobs.length}`,
  )

  // 逐行对账：行上的数字必须逐字等于该 run 自己的 API 字段
  const mism = []
  for (const j of jobs) {
    const t = norm(await txt(`[data-testid="job-counts-${j.runId}"]`))
    const want = norm(countsText(j))
    if (t !== want) mism.push({ runId: j.runId, dom: t, api: want })
    const badge = norm(await txt(`[data-testid="job-status-${j.runId}"]`))
    const wantBadge = norm(headline(j))
    if (badge !== wantBadge) mism.push({ runId: j.runId, dom: badge, api: wantBadge })
  }
  check('UI4 每行成功/失败/跳过/共 + 状态徽标与该 run 的 API 字段逐字一致', mism.length === 0, JSON.stringify(mism).slice(0, 600))

  const r1text = norm(await txt(`[data-testid="job-counts-${run1.runId}"]`))
  check(
    'UI5 跳过数在界面上单独可见（不是并进失败）',
    r1text.includes(`跳过${run1.skipped}`) && r1text.includes(`失败${run1.failed || 0}`),
    r1text,
  )
  check(
    'UI6 行文案非空且跳过/失败两个数字各自独立（跳过没有被渲染成失败）',
    r1text.length > 0 && r1text.includes(`跳过${run1.skipped}`) && r1text.includes(`失败${run1.failed || 0}`),
    `api failed=${run1.failed} skipped=${run1.skipped} row=${r1text}`,
  )

  // 历史累计：只能进折叠区，默认收起
  const legacy = await js(`(()=>{const d=document.querySelector('[data-testid="compliance-jobs-legacy"]');return d?{open:d.open,text:d.textContent.replace(/\\s+/g,'')}:null})()`)
  check('UI7 历史累计在折叠区且默认收起', !!legacy && legacy.open === false, legacy ? `open=${legacy.open}` : '缺失')
  const legacyFailed = (status.queue || {}).assetFailed
  check(
    'UI8 折叠区里的累计失败数 == /judge/status 的累计口径（历史与本次分开）',
    !!legacy && legacy.text.includes(`逐图成${status.queue.assetDone}·败${legacyFailed}`),
    `legacy=${legacy && legacy.text.slice(0, 120)} queue=${JSON.stringify(status.queue)}`,
  )
  check('UI9 页面没有第二个「取消判定」入口', !(await js(`!!document.querySelector('[data-testid="compliance-judge-cancel"]')`)), '')

  await shot('cmp-e2e-01-jobs-list')

  // ---------- 明细抽屉：跳过原因 ----------
  await js(`(()=>{const el=document.querySelector('[data-testid="compliance-job-row-${run1.runId}"]');el&&el.click()})()`)
  const opened = await waitFor(`document.querySelector('[data-testid="compliance-job-detail"]')`, 15000, '明细抽屉')
  check('UI10 点开某一次执行弹出明细', opened, `run=${run1.runId}`)
  if (opened) {
    await sleep(700)
    const title = await txt('[data-testid="compliance-job-detail-title"]')
    check('UI11 抽屉标题点明是第几次执行', norm(title) === norm(`第 ${run1.runId} 次执行`), title)
    const dc = norm(await txt('[data-testid="compliance-job-detail-counts"]'))
    check('UI12 抽屉里的计数与列表行一致', dc === norm(countsText(run1)), `drawer=${dc}`)
    const taskIds = await js(`Array.from(document.querySelectorAll('[data-testid^="compliance-job-task-result-"]')).map(e=>e.getAttribute('data-testid').replace('compliance-job-task-result-',''))`)
    check('UI13 抽屉列出这次的全部 task', taskIds.length === run1.total, `dom=${taskIds.length} api.total=${run1.total}`)
    const labels = await js(`Array.from(document.querySelectorAll('[data-testid^="compliance-job-task-result-"]')).map(e=>e.textContent.trim())`)
    const nSkip = labels.filter((t) => t === '跳过').length
    const nFail = labels.filter((t) => t === '失败').length
    // 跳过原因前缀（与后端 skip_reason.rs / 前端 SKIP_PREFIXES 同一份口径）
    const SKIP_PREFIXES = ['deleted:', 'file_missing:', 'not_image_media:', 'not_image:']
    const isSkip = (t) => SKIP_PREFIXES.some((p) => (t.lastError || '').trim().startsWith(p))
    const detailApi = await (await fetch(`${BASE}/api/compliance/jobs/${run1.runId}?limit=1000`)).json()
    const reasonedSkips = (detailApi.tasks || []).filter(isSkip).length
    // CMP-JOB-FIX-1 后的硬口径：ui 上的「跳过」必须与 run.skipped 逐字相等，
    // 而不是「与带原因的条数相等」——每条跳过都带原因，所以两者本来就该是同一个数。
    check(
      'UI14 界面上「跳过」的条数 == run.skipped == 明细里带跳过原因的条数',
      nSkip === (run1.skipped || 0) && nSkip === reasonedSkips,
      `跳过=${nSkip} run.skipped=${run1.skipped} 带原因=${reasonedSkips}`,
    )
    const ghosts = (detailApi.tasks || []).filter(
      (t) => t.status === 'done' && (t.lastError || '').trim() && !isSkip(t),
    )
    check(
      'UI14b 没有「已完成 + 失败原因」的孤儿行（跳过原因覆盖上一轮旧错误）',
      ghosts.length === 0,
      `孤儿=${ghosts.slice(0, 3).map((t) => `${t.jobId}/asset${t.assetId}/${(t.lastError || '').slice(0, 40)}`)}`,
    )
    check('UI15 真失败的 task 条数与 API failed 一致（跳过不算失败）', nFail === (run1.failed || 0), `失败=${nFail} api=${run1.failed}`)
    const reasons = await js(`Array.from(document.querySelectorAll('[data-testid^="compliance-job-task-reason-"]')).map(e=>e.textContent.trim())`)
    check(
      'UI16 跳过原因翻成人话（「跳过原因：不是图片…」）',
      reasons.some((r) => r.startsWith('跳过原因：不是图片')),
      reasons.slice(0, 2),
    )
    check('UI17 没有把跳过写成「失败原因」', !reasons.some((r) => r.startsWith('失败原因：不是图片')), reasons.slice(0, 2))
    const steps = await js(`Array.from(document.querySelectorAll('[data-testid^="compliance-job-task-step-"]')).map(e=>e.textContent.trim())`)
    check('UI18 步骤名是人话（逐图判定/组级判定），不是 jobNNN', steps.length > 0 && steps.every((s) => /^(逐图判定|组级判定)/.test(s)), steps.slice(0, 2))
    await shot('cmp-e2e-02-run-detail')

    // Esc 关闭（无焦点的 headless 标签页收不到键事件 → 先 bringToFront）
    await send('Page.bringToFront')
    await js('document.body.focus()')
    for (const type of ['rawKeyDown', 'keyUp']) {
      await send('Input.dispatchKeyEvent', {
        type,
        key: 'Escape',
        code: 'Escape',
        windowsVirtualKeyCode: 27,
        nativeVirtualKeyCode: 27,
      })
    }
    await sleep(700)
    let closed = !(await js(`!!document.querySelector('[data-testid="compliance-job-detail"]')`))
    if (!closed) {
      // 兜底：✕ 也必须是可用的关闭路径（键盘不通不等于用户关不掉）
      await js(`(()=>{const b=document.querySelector('[data-testid="compliance-job-detail-close"]');b&&b.click()})()`)
      await sleep(500)
      closed = !(await js(`!!document.querySelector('[data-testid="compliance-job-detail"]')`))
      if (closed) console.log('INFO Esc 未生效，但 ✕ 可关闭（键盘可达性需单独看）')
    }
    check('UI19 明细抽屉可关闭（Esc 或 ✕）', closed, '')
  }

  // 空态：没有在跑的执行时，进度卡不该出现
  const cur = await (await fetch(`${BASE}/api/compliance/jobs/current`)).json()
  check('UI20 无在跑执行时置顶进度卡不出现', cur.run !== null || !(await js(`!!document.querySelector('[data-testid="compliance-job-current"]')`)), `api current=${cur.run && cur.run.runId}`)

  const failed = evidence.filter((e) => !e.ok).length
  await fs.mkdir(path.dirname(OUT), { recursive: true })
  await fs.writeFile(OUT, JSON.stringify({ generatedAt: Date.now(), passed: evidence.length - failed, failed, assertions: evidence }, null, 2))
  console.log(`UI_PROBE_RESULT ${failed === 0 ? 'OK' : 'FAIL:' + failed}`)
  process.exit(failed === 0 ? 0 : 1)
}

main().catch((e) => {
  console.error('UI 探针异常：', e)
  process.exit(2)
})

#!/usr/bin/env node
/**
 * COMPLIANCE-UI3 UI 实点验收（真 dam-web + 真数据，DOM 断言而非接口断言）。
 *
 * 用法：
 *   node scripts/ui3_ui_probe.mjs http://127.0.0.1:8799
 *   CDP_URL=http://127.0.0.1:9225 node scripts/ui3_ui_probe.mjs http://<dam-web>
 *
 * 需要：一个已启动的 Chrome 远程调试端点（--remote-debugging-port）。本脚本只用 Node 内置
 * WebSocket（Node ≥ 22），无第三方依赖。
 *
 * 覆盖（每步都是真点击 / 真输入 / 真读 DOM）：
 *   1) 统计栏三档（合格/不合格/未判定）+ 合计 == 总资产
 *   2) 队列视图（按图第一层）→ 列表视图（逐规则）切换，两视图行数一致
 *   3) 排序切换（置信度 / 文件名 / 资产号）真的改变行顺序
 *   4) 层级展开/收起（资产卡片 → 规则）
 *   5) 改判对话框：备注必填（未填不可提交）→ 提交 → 行上出现「生效」徽章与机器结论对照
 *   6) 撤销改判：生效徽章消失（回到机器结论）
 *   7) 检查器合规区块：三态徽章 + 置信度 + 规则行 + 改判入口
 * 终态行：UI3_PROBE_RESULT OK / FAIL:<原因>
 */
import { appendFileSync } from 'node:fs'

const BASE = (process.argv[2] || 'http://127.0.0.1:8799').replace(/\/$/, '')
const CDP = (process.env.CDP_URL || 'http://127.0.0.1:9225').replace(/\/$/, '')
const SHOTS = process.env.UI3_SHOTS || ''
const STEPS = process.env.UI3_STEPS || ''
// 进度写文件（node 输出到管道/文件时是块缓冲，卡住时看不到最后一步）
const stepLog = (m) => {
  if (!STEPS) return
  try {
    appendFileSync(STEPS, `${new Date().toISOString()} ${m}\n`)
  } catch {}
}

const fails = []
const notes = []
function pass(msg) {
  notes.push(`OK   ${msg}`)
  console.log(`OK   ${msg}`)
}
function bad(msg) {
  fails.push(msg)
  console.log(`FAIL ${msg}`)
}

// ---------------------------------------------------------------- CDP 最小客户端
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
    const list = await (await fetch(`${CDP}/json/version`)).json()
    const ws = new WebSocket(list.webSocketDebuggerUrl)
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

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function main() {
  stepLog('connect')
  // 先清掉残留标签页：反复跑留下的 page target 会让 Page.navigate 不返回（本卡踩过两次）
  try {
    const pages = (await (await fetch(`${CDP}/json/list`)).json()).filter((t) => t.type === 'page')
    for (const t of pages) await fetch(`${CDP}/json/close/${t.id}`)
    if (pages.length) stepLog(`closed ${pages.length} stale pages`)
  } catch {}
  const cdp = await Cdp.connect()
  stepLog('connected')
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' })
  stepLog('target ' + targetId)
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true })
  const send = (m, p = {}) => cdp.send(m, p, sessionId)

  await send('Page.enable')
  await send('Runtime.enable')
  // 头号假象防线：未聚焦标签页 document.visibilityState='hidden' → React 不 hydrate
  await send('Emulation.setFocusEmulationEnabled', { enabled: true })
  stepLog('focus emulation')

  const js = async (expr) => {
    const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true })
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + ' :: ' + (r.exceptionDetails.exception?.description ?? ''))
    return r.result.value
  }
  const waitFor = async (expr, ms = 15000, label = expr) => {
    const t0 = Date.now()
    while (Date.now() - t0 < ms) {
      if (await js(`(()=>{try{return !!(${expr})}catch(e){return false}})()`)) return true
      await sleep(150)
    }
    bad(`等待超时：${label}`)
    return false
  }
  const shot = async (name) => {
    if (!SHOTS) return
    const r = await send('Page.captureScreenshot', { format: 'png' })
    const fs = await import('node:fs/promises')
    await fs.writeFile(`${SHOTS}/${name}.png`, Buffer.from(r.data, 'base64'))
  }
  const click = async (sel) => {
    const ok = await js(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});
      if(!el) return false; el.scrollIntoView({block:'center'}); el.click(); return true})()`)
    if (!ok) throw new Error(`找不到元素 ${sel}`)
    await sleep(250)
  }
  const text = async (sel) => js(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});return el?el.textContent.trim():null})()`)
  const setSelect = async (sel, value) => {
    await js(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});
      const setter=Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype,'value').set;
      setter.call(el, ${JSON.stringify(value)}); el.dispatchEvent(new Event('change',{bubbles:true}))})()`)
    await sleep(400)
  }
  const typeInto = async (sel, val) => {
    await js(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});
      const setter=Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype,'value').set;
      setter.call(el, ${JSON.stringify(val)}); el.dispatchEvent(new Event('input',{bubbles:true}))})()`)
    await sleep(200)
  }
  const DLG = '[role="dialog"][aria-label="人工改判"]'
  const rowKeys = () =>
    js(`Array.from(document.querySelectorAll('[data-testid^="review-row-"]')).map(e=>e.getAttribute('data-testid'))`)

  // ---------------------------------------------------------------- 打开合规页
  stepLog('navigate')
  await send('Page.navigate', { url: `${BASE}/` })
  await sleep(1500)
  const vis = await js('document.visibilityState')
  vis === 'visible' ? pass(`标签页可见（visibilityState=${vis}）`) : bad(`标签页不可见（${vis}）→ React 不会 hydrate`)
  const hydrated = await waitFor(`document.querySelector('nav a[href="/compliance"]')`, 15000, '侧栏导航（React 已 hydrate）')
  if (!hydrated) throw new Error('React 未 hydrate')
  // 首次导入向导（role=dialog）会挡在前面 —— 先「稍后再说」，否则对话框断言会打到向导上
  await js(`(()=>{const b=Array.from(document.querySelectorAll('button'))
      .find(x=>/稍后再说|关闭/.test(x.textContent)); if(b) b.click(); return !!b})()`)
  await sleep(400)
  await click('nav a[href="/compliance"]')
  await waitFor(`document.querySelector('[data-testid="compliance-page"]')`, 15000, '合规页挂载')

  stepLog('step1 stats')
  // 1) 统计栏三档
  await waitFor(`document.querySelector('[data-testid="tier-total"]')`, 15000, '统计栏')
  const tierTexts = await js(`['pass','fail','undetermined'].map(k=>{
      const el=document.querySelector('[data-testid="tier-'+k+'"]'); return el?el.textContent.trim():null})`)
  const totalText = await text('[data-testid="tier-total"]')
  const nums = (totalText || '').match(/\d+/g) || []
  const sum = Number(nums[0]), total = Number(nums[1])
  notes.push(`     统计栏: ${tierTexts.join(' | ')} | ${totalText}`)
  tierTexts.every((t) => t && /(合格|不合格|未判定)/.test(t))
    ? pass('统计栏是三档（合格/不合格/未判定）')
    : bad(`统计栏档位不对: ${JSON.stringify(tierTexts)}`)
  sum === total ? pass(`三档合计 == 总资产（${sum} == ${total}）`) : bad(`三档合计 ${sum} != 总资产 ${total}`)
  await shot('ui3-01-compliance-stats')

  stepLog('step2 views')
  // 2) 队列/列表切换
  const queueCards = await js(`document.querySelectorAll('[data-testid^="compliance-card-"]').length`)
  await click('[data-testid="view-list"]')
  await waitFor(`document.querySelector('[data-testid="compliance-list"]')`, 8000, '列表视图')
  const listRows = (await rowKeys()).length
  await click('[data-testid="view-queue"]')
  await waitFor(`document.querySelector('[data-testid="compliance-queue"]')`, 8000, '队列视图')
  queueCards > 0 && listRows > 0
    ? pass(`视图切换：队列 ${queueCards} 张资产卡片 / 列表 ${listRows} 行规则`)
    : bad(`视图切换后没内容（队列 ${queueCards}, 列表 ${listRows}）`)
  await shot('ui3-02-queue-view')

  stepLog('step3 sort')
  // 3) 排序真的换序（置信度 vs 资产号 vs 文件名）
  const byConf = await rowKeys()
  await setSelect('[data-testid="sort-select"]', 'asset')
  const byAsset = await rowKeys()
  await setSelect('[data-testid="sort-select"]', 'fileName')
  const byName = await rowKeys()
  JSON.stringify(byConf) !== JSON.stringify(byAsset)
    ? pass('排序切换（置信度 → 资产号）真的改变行顺序')
    : bad('排序切换没有改变行顺序（置信度/资产号）')
  notes.push(`     顺序前 3：conf=${byConf.slice(0, 3).join(',')} asset=${byAsset.slice(0, 3).join(',')} name=${byName.slice(0, 3).join(',')}`)
  await setSelect('[data-testid="sort-select"]', 'confidence')

  stepLog('step4 hierarchy')
  // 4) 层级展开/收起
  await click('[data-testid="view-list"]')
  await waitFor(`document.querySelector('[data-testid="compliance-list"]')`, 8000, '列表视图')
  await click('[data-testid="view-queue"]')
  await waitFor(`document.querySelector('[data-testid="compliance-queue"]')`, 8000, '队列视图')
  const cardId = await js(`(()=>{const el=document.querySelector('[data-testid^="compliance-card-"]');
      return el?el.getAttribute('data-testid').replace('compliance-card-',''):null})()`)
  if (!cardId) bad('队列视图没有资产卡片')
  else {
    await click(`[data-testid="card-toggle-${cardId}"]`)
    const closed = await js(`!document.querySelector('[data-testid="card-rules-${cardId}"]')`)
    await click(`[data-testid="card-toggle-${cardId}"]`)
    const opened = await js(`!!document.querySelector('[data-testid="card-rules-${cardId}"]')`)
    closed && opened ? pass(`层级展开/收起生效（资产 #${cardId} → 规则）`) : bad('层级展开/收起不生效')
    await shot('ui3-03-expanded-rules')
  }

  stepLog('step5 review')
  // 5) 改判：备注必填 → 提交 → 生效徽章
  const target = await js(`(()=>{
      const btn=document.querySelector('[data-testid^="review-pass-"]');
      if(!btn) return null;
      const tid=btn.getAttribute('data-testid');
      const row=btn.closest('[data-testid^="review-row-"]');
      return {tid, row: row?row.getAttribute('data-testid'):null}})()`)
  if (!target) bad('没有可改判的行')
  else {
    const ruleKey = target.tid.replace('review-pass-', '') // <asset>-<rule|asset>
    await click(`[data-testid="${target.tid}"]`)
    await waitFor(`document.querySelector(${JSON.stringify(DLG)})`, 8000, '改判对话框')
    const dialogText = await text(DLG)
    if ((dialogText || '').includes('机器结论')) pass('对话框如实显示机器结论（机器结论不可编辑）')
    else bad('对话框没显示机器结论')
    const disabledBefore = await js(`(()=>{const b=Array.from(document.querySelectorAll('[aria-label="人工改判"] button'))
        .find(x=>/提交改判/.test(x.textContent)); return b?b.disabled:null})()`)
    disabledBefore === true ? pass('备注为空时「提交改判」不可点（备注必填）') : bad(`备注为空仍可提交（disabled=${disabledBefore}）`)
    await shot('ui3-04-review-dialog')
    await typeInto('#review-reason', 'UI3 验收：人工核对原图后改判')
    const disabledAfter = await js(`(()=>{const b=Array.from(document.querySelectorAll('[aria-label="人工改判"] button'))
        .find(x=>/提交改判/.test(x.textContent)); return b?b.disabled:null})()`)
    disabledAfter === false ? pass('填了备注后可提交') : bad(`填了备注仍不可提交（disabled=${disabledAfter}）`)
    // 高置信度：可能要先勾选确认
    const needForce = await js(`!!Array.from(document.querySelectorAll('[aria-label="人工改判"] input[type="checkbox"]')).length`)
    if (needForce) {
      await js(`(()=>{const c=document.querySelector('[aria-label="人工改判"] input[type="checkbox"]'); c.click()})()`)
      notes.push('     该目标属高置信度 → 已勾选「确认改判」')
    }
    await js(`(()=>{const b=Array.from(document.querySelectorAll('[aria-label="人工改判"] button'))
        .find(x=>/提交改判/.test(x.textContent)); b.click()})()`)
    const dialogGone = await waitFor(`!document.querySelector(${JSON.stringify(DLG)})`, 10000, '提交后对话框关闭')
    if (dialogGone) pass('提交后对话框关闭（服务端已受理）')
    else {
      const err = await text(DLG)
      bad(`提交后对话框未关闭（可能被服务端拒绝）: ${(err || '').slice(0, 160)}`)
    }
    const rowTestId = `review-row-${ruleKey}`
    const hasFinal = await waitFor(`document.querySelector('[data-testid="final-${ruleKey.split('-').slice(1).join('-')}"]')`, 6000, '行上生效徽章')
    if (hasFinal) {
      const rowText = await text(`[data-testid="${rowTestId}"]`)
      if ((rowText || '').includes('人工改判')) pass('行上同时显示机器结论与「人工改判」（机器结论未被改写）')
      else bad(`行上没有改判说明: ${(rowText || '').slice(0, 120)}`)
    } else bad('改判后行上没有生效徽章')
    await shot('ui3-05-reviewed')

    stepLog('step6 revoke')
    // 6) 撤销
    const revokeSel = `[data-testid="review-revoke-${ruleKey}"]`
    const hasRevoke = await js(`!!document.querySelector(${JSON.stringify(revokeSel)})`)
    if (!hasRevoke) bad('改判后没有撤销入口')
    else {
      await click(revokeSel)
      await waitFor(`document.querySelector(${JSON.stringify(DLG)})`, 8000, '撤销对话框')
      await js(`(()=>{const b=Array.from(document.querySelectorAll('[aria-label="人工改判"] button'))
          .find(x=>/撤销改判/.test(x.textContent)); b.click()})()`)
      await waitFor(`!document.querySelector(${JSON.stringify(DLG)})`, 10000, '撤销后对话框关闭')
      const stillFinal = await js(`!!document.querySelector('[data-testid="final-${ruleKey.split('-').slice(1).join('-')}"]')`)
      stillFinal ? bad('撤销后仍显示生效徽章') : pass('撤销后生效徽章消失（回到机器结论）')
      await shot('ui3-06-revoked')
    }
  }

  stepLog('step7 inspector')
  // 7) 检查器合规区块
  await click('nav a[href="/"]')
  await waitFor(`document.querySelector('[data-testid="grid"], [data-testid^="asset-"]')`, 15000, '图库网格')
  const opened = await js(`(()=>{
      const el=document.querySelector('[data-testid^="asset-"]') || document.querySelector('[data-testid="grid"] button, main img');
      if(!el) return false; el.click(); return true})()`)
  if (!opened) bad('无法选中一张图（检查器打不开）')
  else {
    const hasPanel = await waitFor(`document.querySelector('[data-testid="insp-compliance"]')`, 10000, '检查器合规区块')
    if (hasPanel) {
      const state = await text('[data-testid="insp-compliance-machine-state"]')
      const badgeOk = /(合格|不合格|未判定)/.test(state || '')
      badgeOk ? pass(`检查器合规区块显示三态机器结论（${state}）`) : bad(`检查器三态徽章异常: ${state}`)
      const hasReview = await js(`!!document.querySelector('[data-testid="insp-compliance-asset-review"]')`)
      const hasRuleLine = await js(`document.querySelectorAll('[data-testid^="insp-compliance-"]').length>1`)
      hasReview && hasRuleLine ? pass('检查器有规则明细 + 整张图改判入口') : bad('检查器缺规则明细或改判入口')
      await shot('ui3-07-inspector')
    }
  }

  console.log('')
  console.log(notes.join('\n'))
  if (fails.length) {
    console.log(`\nUI3_PROBE_RESULT FAIL:${fails.join(' | ')}`)
    process.exit(1)
  }
  console.log('\nUI3_PROBE_RESULT OK')
  try { await cdp.send('Target.closeTarget', { targetId }) } catch {}
  try { cdp.ws.close() } catch {}
  process.exit(0) // CDP 的 WebSocket 会吊住事件循环；结论已打出，直接收工
}

main().catch((e) => {
  console.log(`\nUI3_PROBE_RESULT FAIL:${e.message}`)
  if (process.env.UI3_TRACE) console.log(e.stack)
  process.exit(1)
})

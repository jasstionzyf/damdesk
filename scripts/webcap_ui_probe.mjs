/**
 * WEBCAP 验收：UI 灰置的 **CDP DOM 取证**（卡 t_43212e61）。
 *
 * 跑在容器内（Node ≥22，用内置 `fetch` + 全局 `WebSocket`，不引任何依赖）：
 *   CDP_PORT=9229 APP_URL=http://127.0.0.1:8891 OUT=/tmp/webcap-ui.json \
 *     node scripts/webcap_ui_probe.mjs
 *
 * 前置：同容器内已跑起 chrome-headless-shell（--remote-debugging-port=9229）。
 *
 * 断言（网页容器形态）：
 *   1. 页面真的 hydrate 了（#root 有内容、visibilityState=visible）——headless 非前置标签
 *      会 visibility=hidden → React 不 hydrate，看起来像「界面不存在」，必须先排掉这种假象
 *   2. `[data-testid=move-select]` / `[data-testid=move-apply]` 都是 disabled
 *   3. `[data-testid=move-disabled-reason]` 可见且文案非空（且就是服务端 featureReasons 那句）
 *   4. 目录树行（拖放「移动到目录」目标）带 `data-drop-disabled=1` + title 含人话原因
 *   5. 截图存 OUT 同目录（报告可直接点开）
 *
 * 退出码 0 = 全 PASS；证据 JSON 写 OUT（报告摘原文）。
 */
import fs from 'node:fs'
import path from 'node:path'

const PORT = process.env.CDP_PORT || '9229'
const APP_URL = process.env.APP_URL || 'http://127.0.0.1:8891'
const OUT = process.env.OUT || '/tmp/webcap-ui.json'
const SHOT = process.env.SHOT || path.join(path.dirname(OUT), 'webcap-ui.png')
// disabled = 网页容器形态（入口必须灰置 + reason 非空）
// enabled  = 桌面形态钩子（入口可用；但只读库根的目录行仍不可拖放 —— ③ 逐目录限定）
const EXPECT = process.env.EXPECT || 'disabled'
// 可写根目录 / 只读根目录（容器卷约定，与 webcap_http_asserts.py 一致）
const LIB = process.env.WEBCAP_LIB || '/data/webcap-lib'
const CORPUS = process.env.WEBCAP_CORPUS || '/corpus/batch1'

let pass = 0
let fail = 0
function check(name, ok, detail) {
  if (ok) {
    pass++
    console.log(`PASS ${name} :: ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`)
  } else {
    fail++
    console.log(`FAIL ${name} :: ${typeof detail === 'string' ? detail : JSON.stringify(detail)}`)
  }
  return ok
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function newTarget(url) {
  const r = await fetch(`http://127.0.0.1:${PORT}/json/new?${encodeURIComponent(url)}`, { method: 'PUT' })
  if (!r.ok) throw new Error(`/json/new ${r.status}: ${await r.text()}`)
  return r.json()
}

/** 极简 CDP 客户端（直连 page target，不搞 session 多路复用）。 */
function connect(wsUrl) {
  const ws = new WebSocket(wsUrl)
  const pending = new Map()
  let seq = 0
  ws.addEventListener('message', (ev) => {
    let m
    try {
      m = JSON.parse(ev.data)
    } catch {
      return
    }
    if (m.id && pending.has(m.id)) {
      pending.get(m.id)(m)
      pending.delete(m.id)
    }
  })
  const ready = new Promise((res, rej) => {
    ws.addEventListener('open', () => res())
    ws.addEventListener('error', (e) => rej(new Error('ws error ' + (e?.message || ''))))
  })
  function send(method, params = {}) {
    const id = ++seq
    ws.send(JSON.stringify({ id, method, params }))
    return new Promise((res) => pending.set(id, res))
  }
  async function evaluate(expression) {
    const r = await send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise: true,
      userGesture: true,
    })
    if (r.error) throw new Error(JSON.stringify(r.error))
    const ex = r.result?.exceptionDetails
    if (ex) throw new Error(`eval exception: ${ex.text} ${ex.exception?.description || ''}`)
    return r.result?.result?.value
  }
  return { ws, ready, send, evaluate }
}

async function waitFor(ev, expr, timeoutMs = 30000, label = expr) {
  const t0 = Date.now()
  while (Date.now() - t0 < timeoutMs) {
    const v = await ev(expr)
    if (v) return v
    await sleep(250)
  }
  throw new Error(`timeout waiting: ${label}`)
}

const main = async () => {
  const target = await newTarget(APP_URL)
  const { ws, ready, send, evaluate } = connect(target.webSocketDebuggerUrl)
  await ready
  await send('Page.enable')
  await send('Runtime.enable')
  // headless 非前置标签会 visibility=hidden（React 不 hydrate）→ 强制置前 + 焦点模拟
  await send('Emulation.setFocusEmulationEnabled', { enabled: true })
  await send('Page.bringToFront')

  // 首帧：等 hydrate（#root 有子节点）
  await waitFor(evaluate, '!!document.querySelector("#root") && document.querySelector("#root").children.length > 0',
    30000, '#root hydrate')

  // 首启引导向导会盖住界面 → 明确标记「已向导过」再重载（等价用户点过「稍后导入」）
  const onboarded = await evaluate('localStorage.getItem("dam.onboarded")')
  if (onboarded !== '1') {
    await evaluate('localStorage.setItem("dam.onboarded","1"), true')
    await send('Page.reload', { ignoreCache: false })
    await sleep(500)
    await waitFor(evaluate, '!!document.querySelector("#root") && document.querySelector("#root").children.length > 0',
      30000, '#root hydrate after reload')
  }

  const vis = await evaluate('document.visibilityState')
  check('页面可见（visibilityState=visible，排除 headless 冻结假象）', vis === 'visible', vis)

  // 图库页要有资产卡片（seed 造的 4 张）
  await waitFor(evaluate, 'document.querySelectorAll("[data-testid^=\\"asset-card-\\"]").length > 0',
    30000, 'asset cards')
  const cards = await evaluate('document.querySelectorAll("[data-testid^=\\"asset-card-\\"]").length')
  check('图库渲染出资产卡片', cards > 0, `cards=${cards}`)

  // 单选 → 检查器（MovePicker 就在检查器里）
  await evaluate('(() => { const c = document.querySelector("[data-testid^=\\"asset-card-\\"]"); c.click(); return true })()')
  await waitFor(evaluate, '!!document.querySelector("[data-testid=move-select]")', 20000, 'move-select')

  const probe = await evaluate(`(() => {
    const sel = document.querySelector('[data-testid=move-select]')
    const btn = document.querySelector('[data-testid=move-apply]')
    const reason = document.querySelector('[data-testid=move-disabled-reason]')
    const rows = [...document.querySelectorAll('[data-testid^="drop-dir-"]')].map((r) => ({
      testid: r.getAttribute('data-testid'),
      dropDisabled: r.getAttribute('data-drop-disabled'),
      ariaDisabled: r.getAttribute('aria-disabled'),
      title: r.getAttribute('title'),
    }))
    return {
      url: location.href,
      visibility: document.visibilityState,
      moveSelect: sel ? { disabled: sel.disabled, title: sel.getAttribute('title'), html: sel.outerHTML.slice(0, 400) } : null,
      moveApply: btn ? { disabled: btn.disabled, title: btn.getAttribute('title'), html: btn.outerHTML.slice(0, 400) } : null,
      reasonText: reason ? reason.textContent.trim() : null,
      reasonVisible: reason ? reason.getBoundingClientRect().height > 0 : false,
      dirRows: rows,
      options: [...document.querySelectorAll('[data-testid=move-select] option')].map((o) => ({ value: o.value, disabled: o.disabled, text: o.textContent })),
    }
  })()`)

  fs.mkdirSync(path.dirname(OUT), { recursive: true })
  fs.writeFileSync(OUT, JSON.stringify(probe, null, 2))

  // ---- 断言：按形态分别校验入口状态 ----
  const rowOf = (dir) => probe.dirRows.find((r) => r.testid === `drop-dir-${dir}`)
  const optOf = (dir) => (probe.options || []).find((o) => o.value === dir)

  if (EXPECT === 'disabled') {
    check('检查器「移动到目录」下拉存在', !!probe.moveSelect, JSON.stringify(probe.moveSelect))
    check('下拉 disabled（形态不支持磁盘移动）', probe.moveSelect?.disabled === true, JSON.stringify(probe.moveSelect))
    check('「移动」按钮 disabled', probe.moveApply?.disabled === true, JSON.stringify(probe.moveApply))
    check('下拉带非空 title（人话原因 tooltip）', (probe.moveSelect?.title || '').length > 10, probe.moveSelect?.title || '')
    check(
      'reason 文案可见且非空（服务端 featureReasons 原句）',
      probe.reasonVisible === true && (probe.reasonText || '').length > 10,
      JSON.stringify({ visible: probe.reasonVisible, text: probe.reasonText }),
    )
    check(
      'reason 指向桌面版 / copy-in（不是黑话）',
      /桌面版|copy-in|复制入库/.test(probe.reasonText || probe.moveSelect?.title || ''),
      probe.reasonText || '',
    )
    check('目录树有目录行', probe.dirRows.length > 0, JSON.stringify(probe.dirRows.map((r) => r.testid)))
    const rowsOk = probe.dirRows.length > 0 && probe.dirRows.every(
      (r) => r.dropDisabled === '1' && r.ariaDisabled === 'true' && (r.title || '').length > 4,
    )
    check('所有目录行 data-drop-disabled=1 + aria-disabled + title 含原因（不可拖放移动）', rowsOk,
      JSON.stringify(probe.dirRows))
    check('所有目录选项 disabled（含「只读，不可移」标注）',
      (probe.options || []).filter((o) => o.value).every((o) => o.disabled && /只读，不可移/.test(o.text)),
      JSON.stringify(probe.options))
  } else {
    check('下拉 enabled（桌面形态支持磁盘移动）', probe.moveSelect?.disabled === false, JSON.stringify(probe.moveSelect))
    check('无 disabled reason 提示块（能力可用时不误报）', probe.reasonText === null, String(probe.reasonText))
    const libRow = rowOf(LIB)
    const corpusRow = rowOf(CORPUS)
    check('可写库根目录行可拖放（无 data-drop-disabled）',
      !!libRow && libRow.dropDisabled === null, JSON.stringify(probe.dirRows))
    check('只读库根目录行仍不可拖放 + title 含原因（③ 逐目录限定）',
      !!corpusRow && corpusRow.dropDisabled === '1' && (corpusRow.title || '').length > 4,
      JSON.stringify(probe.dirRows))
    check('只读根选项 disabled + 「只读，不可移」，可写根选项可选',
      optOf(CORPUS)?.disabled === true && optOf(LIB)?.disabled === false,
      JSON.stringify(probe.options))
  }

  // ---- 截图存证 ----
  const shot = await send('Page.captureScreenshot', { format: 'png' })
  if (shot.result?.data) {
    fs.writeFileSync(SHOT, Buffer.from(shot.result.data, 'base64'))
    console.log(`SHOT ${SHOT}`)
  }

  console.log(`DOM 片段（move-select）: ${probe.moveSelect?.html || '(null)'}`)
  console.log(`DOM 片段（reason）: ${probe.reasonText || '(null)'}`)

  ws.close()
  console.log(`---- ui PASS=${pass} FAIL=${fail} evidence=${OUT}`)
  process.exit(fail === 0 ? 0 : 1)
}

main().catch((e) => {
  console.error('PROBE ERROR', e)
  process.exit(2)
})

// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { ComplianceReviewDialog } from '../src/components/ComplianceReviewDialog'

// i18n-C（卡 task）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
// jsdom 的 navigator.language 默认 en-US（会渲染英文）。这里把语言钉成 zh-Hans ——
// t() 在中文下缺译回退 key（= 中文原文），断言逐字不变、强度不降，只是显式声明
// 「本文件验中文文案」。vi.hoisted 早于所有 import 执行，故早于 i18n 模块初始化。
vi.hoisted(() => {
  // jsdom 文件有真实 localStorage；node 环境的纯 SSR 渲染测试没有它 ——
  // 给一个最小桩，目的只有一个：让 i18n 初始化时 resolveInitialLocale() 读到 zh-Hans。
  try {
    if (typeof localStorage !== 'undefined' && localStorage) {
      localStorage.setItem('dam.locale', 'zh-Hans')
      return
    }
  } catch {
    /* 落桩 */
  }
  vi.stubGlobal('localStorage', {
    getItem: (k: string) => (k === 'dam.locale' ? 'zh-Hans' : null),
    setItem: () => {},
    removeItem: () => {},
    clear: () => {},
    key: () => null,
    length: 0,
  })
})

/**
 * KI-8b（卡 task）回归：把「状态页 / 弹窗 / 顶栏」的手写按钮收敛到统一载体。
 *
 * 与 buttonCarrier.dom.test.tsx（KI-8a，载体本身）互补：那边证明 `Button` 四种载体
 * 在静止态可辨，这边守住**收敛是否真的发生**，用的是棘轮（ratchet）而不是一次性断言：
 * - 本卡 17 个文件里不许再出现「非白名单的手写 <button>」—— 新写一个裸按钮就把测试打红；
 * - 白名单里的少数几处是**非按钮载体**（菜单行 / 卡片 / 分段统计 / 评分星级 / 内含子按钮的列表行），
 *   逐条带签名，签名不在了也算收敛（测试仍绿）；
 * - 按钮字色不许是 text-ink-dim（灰=次要信息，不是可点信号，skill soujpg-ui-patterns §9.8）；
 * - 每屏 primary 数量不超过登记值（多出来的主按钮会让强调失效）。
 *
 * 只从源码里读「结构性事实」（哪几个手写按钮还在、谁用了什么字色），视觉判据仍在载体单测与
 * 真机走查里验，避免把 className 文案钉死当行为证据。
 */

const SRC = resolve(__dirname, '../src')

/** 本卡负责的文件（卡体「允许修改的文件」清单，原 17 个）
 *  卡 task 撤销 pages/ExportPage.tsx（导出收编为全局 Dialog）→ 现 16 个 */
const CARD_FILES = [
  'pages/Search.tsx',
  'pages/TagTasks.tsx',
  'pages/TagReview.tsx',
  'pages/Tasks.tsx',
  'pages/Trash.tsx',
  'pages/Settings.tsx',
  'components/TaskCenter.tsx',
  'components/OnboardingWizard.tsx',
  'components/PackManager.tsx',
  'components/SmartFolderDialog.tsx',
  'components/SmartFolderSection.tsx',
  'components/ShortcutsHelp.tsx',
  'components/Lightbox.tsx',
  'components/ExportDialog.tsx',
  'components/ComplianceReviewDialog.tsx',
] as const

/**
 * 允许保留手写 <button> 的站点：每项是「签名（该开标签里必须出现的子串）」+ 为什么它不是按钮。
 * 签名不匹配的手写按钮 = 回归，测试直接失败。
 */
const RAW_BUTTON_ALLOW: Record<string, { signature: string; why: string }[]> = {
  // 右键菜单项：是列表行（菜单语义），不是按钮腔体
  // （标签候选下拉的选项行已随 task 收进 components/TagPicker.tsx 的 role=option 列表行）
  'pages/Search.tsx': [
    { signature: 'data-testid="context-select"', why: '右键菜单项（菜单行）' },
    { signature: 'data-testid="context-find-similar"', why: '右键菜单项（菜单行）' },
  ],
  // 评分星级：KI-11 已整体移除（Lightbox 不再有星级/拒选区）—— 白名单条目随功能一起删
  // 引导页模式选择：整块卡片可点（含标题+说明），属于选择卡而不是按钮
  'components/OnboardingWizard.tsx': [
    { signature: 'px-4 py-3 text-left', why: '模式选择卡（整卡可点）' },
  ],
  // 智能文件夹列表行：行内还有 ✎/🗑 两个按钮，用 <button> 包 <button> 是非法结构
  'components/SmartFolderSection.tsx': [
    { signature: 'flex min-w-0 flex-1 items-center', why: '列表行载体（内含子按钮，不可嵌套）' },
  ],
}

/** 每屏 primary 数量上限（本卡收敛后的登记值；超出即回归） */
const PRIMARY_CEILING: Record<string, number> = {
  // 搜索：搜索栏主按钮 1 + 「索引不可用」告警卡条件分支 1 + 批量打标浮层 1（不同载体，不同时主导）
  'pages/Search.tsx': 3,
  // 任务页：新建任务主按钮 + 编辑浮层保存
  'pages/TagTasks.tsx': 2,
  // 标签复核：页级只留「全部确认」一个主按钮（逐卡「确认入库」已降为二级）
  'pages/TagReview.tsx': 1,
  'pages/Tasks.tsx': 0,
  'pages/Trash.tsx': 1,
  // 设置页：向量化「保存」+ 模型区「下载全部可选模型」（GH-9：缺件才出现的主 CTA，
  // 缺模型 = 该页最重要的未完成动作；两个 primary 不同时在场——前者常驻、后者条件渲染）
  'pages/Settings.tsx': 2,
  'components/TaskCenter.tsx': 0,
  'components/OnboardingWizard.tsx': 1,
  'components/PackManager.tsx': 0,
  'components/SmartFolderDialog.tsx': 1,
  'components/SmartFolderSection.tsx': 0,
  'components/ShortcutsHelp.tsx': 0,
  'components/Lightbox.tsx': 0,
  'components/ExportDialog.tsx': 1,
  'components/ComplianceReviewDialog.tsx': 1,
}

/**
 * 仍在用手写按钮 + text-ink-dim 的文件：全部**属于别的卡的领地**（KI-8a 后续/其他卡），
 * 本卡不改。这里只做「不许变多」的守卫，等那些卡收敛后此集合自然缩小（子集断言，缩了不会红）。
 */
const KNOWN_OUT_OF_SCOPE_GRAY_BUTTONS = new Set([
  'components/AssetGrid.tsx',
  'components/DensityControl.tsx',
  'components/FilterBanner.tsx',
  'components/LibraryTree.tsx',
])

afterEach(cleanup)

function read(rel: string): string {
  return readFileSync(resolve(SRC, rel), 'utf8')
}

/** 取出所有 `<button ...>` 开标签（含多行属性） */
function openingTags(src: string, tag: string): string[] {
  const out: string[] = []
  const needle = `<${tag}`
  let i = src.indexOf(needle)
  while (i >= 0) {
    let j = i
    while (j < src.length && !(src[j] === '>' && src[j - 1] !== '=')) j++
    out.push(src.slice(i, j + 1))
    i = src.indexOf(needle, j)
  }
  return out
}

describe('KI-8b：手写按钮收敛棘轮（本卡 16 个文件）', () => {
  it('没有白名单之外的手写 <button>（新写裸按钮 = 红）', () => {
    const leftovers: string[] = []
    for (const f of CARD_FILES) {
      const tags = openingTags(read(f), 'button')
      const allow = RAW_BUTTON_ALLOW[f] ?? []
      for (const tag of tags) {
        const hit = allow.find((a) => tag.includes(a.signature))
        if (!hit) leftovers.push(`${f}: ${tag.replace(/\s+/g, ' ').slice(0, 140)}`)
      }
      // 白名单签名必须都真的还在文件里（防止签名漂移后白名单变成摆设）
      for (const a of allow) {
        expect(tags.some((t) => t.includes(a.signature)) || read(f).includes(a.signature), `${f} 白名单签名失效：${a.signature}`).toBe(true)
      }
    }
    expect(leftovers, `这些手写按钮应改用 <Button>（或登记进 RAW_BUTTON_ALLOW 并写明为什么不是按钮）：\n${leftovers.join('\n')}`).toEqual([])
  })

  it('收敛后的按钮不再用 text-ink-dim 当字色（灰字不是可点信号）', () => {
    const bad: string[] = []
    for (const f of CARD_FILES) {
      const src = read(f)
      for (const tag of [...openingTags(src, 'Button'), ...openingTags(src, 'button')]) {
        if (tag.includes('text-ink-dim')) bad.push(`${f}: ${tag.replace(/\s+/g, ' ').slice(0, 140)}`)
      }
    }
    expect(bad, `按钮字色必须用 ink（ink-dim 留给次要信息）：\n${bad.join('\n')}`).toEqual([])
  })

  it('每屏 primary 不超过登记值（主按钮多了强调就失效）', () => {
    const over: string[] = []
    for (const f of CARD_FILES) {
      const n = (read(f).match(/variant="primary"/g) ?? []).length
      const ceiling = PRIMARY_CEILING[f] ?? 0
      if (n > ceiling) over.push(`${f}: primary=${n} > 登记上限 ${ceiling}`)
    }
    expect(over, `主按钮超量：\n${over.join('\n')}`).toEqual([])
  })

  it('页面文件确实在用统一载体（不是「一个按钮都没改」也算过）', () => {
    for (const f of CARD_FILES) {
      const src = read(f)
      // ShortcutsHelp 是纯说明面板，不含动作
      if (f === 'components/ShortcutsHelp.tsx') continue
      expect((src.match(/<Button\b/g) ?? []).length, `${f} 没有使用 <Button> 载体`).toBeGreaterThan(0)
    }
  })

  it('别处仍在用灰字按钮的只剩已知（非本卡）文件，且没有再扩大', () => {
    const gray: string[] = []
    for (const f of CARD_FILES) {
      for (const tag of openingTags(read(f), 'button')) {
        if (tag.includes('text-ink-dim')) gray.push(f)
      }
    }
    expect(gray.filter((f) => !KNOWN_OUT_OF_SCOPE_GRAY_BUTTONS.has(f))).toEqual([])
  })
})

describe('KI-8b：改判对话框的 合格/不合格 走载体不带语义色自绘', () => {
  const target = {
    assetId: 42,
    fileName: 'a.jpg',
    ruleId: null,
    ruleName: null,
    machineResult: 'fail',
    machineConfidence: 0.4,
    threshold: 0.6,
  }

  it('两个选项都是统一载体：可见描边 + ink 字、aria-pressed 跟随选择', () => {
    render(
      <ComplianceReviewDialog
        target={target}
        onSubmit={async () => ({})}
        onRevoke={async () => ({})}
        onClose={() => {}}
        onDone={() => {}}
      />,
    )
    const pass = screen.getByRole('button', { name: '合格' })
    const fail = screen.getByRole('button', { name: '不合格' })
    // 默认选「合格」（机器判 fail → 人工默认 pass）：选中态用 accent-soft 底色承载
    expect(pass.getAttribute('aria-pressed')).toBe('true')
    expect(pass.className).toContain('bg-accent-soft')
    expect(fail.getAttribute('aria-pressed')).toBe('false')
    // 未选中的那一个也必须是「静止态可辨」的载体，而不是灰字裸按钮（危险项走 danger tone：红描边+红字）
    expect(fail.className).toContain('border-red-400/70')
    expect(fail.className).toContain('text-red-300')
    expect(fail.className).toContain('bg-panel-2')
    for (const el of [pass, fail]) expect(el.className).not.toContain('text-ink-dim')
    // 危险选项带红色 tone（不再靠自绘 emerald/rose 表达结论）
    expect(fail.className).toContain('border-red-400/70')
    // 互斥切换：点「不合格」后 aria-pressed 反转，且「合格」回落到二级描边
    fireEvent.click(fail)
    expect(fail.getAttribute('aria-pressed')).toBe('true')
    expect(pass.getAttribute('aria-pressed')).toBe('false')
    expect(pass.className).toContain('border-ink-dim/60')
    expect(pass.className).not.toContain('text-ink-dim')
  })
})

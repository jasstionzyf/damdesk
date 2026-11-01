/**
 * KI-2 / KI-7（卡 t_8f8a23c2）：去重入口与去重页整合的**前端口径**。
 *
 * 覆盖四类「用户会踩到」的点（全部可在单测里核对的 DOM/纯函数）：
 *  1. 主导航不再有「去重」（低频处置动作不进一级位），`/dedup` 页面保留；
 *  2. 侧栏「重复 N 组」徽标只在 N>0 时渲染（0 组不占位、不打扰）；
 *  3. 变体（variant）组没有「一键全处理」语义：文案是「保留这张，其余 N 张…」，
 *     且必须走二次确认（`needsSecondConfirm`）；精确/近似仍是软删 + 撤销窗口；
 *  4. 「仅检查重复（不入库）」入口按能力位 `scanExternalPath` 隐藏（不是灰置）：
 *     导入下拉与命令面板两处同源判定。
 *
 * 还有：三态徽标口径（未检测 ≠ 无重复）、范围标签/范围外重复提示。
 */
import { describe, expect, it, vi } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { MemoryRouter } from 'react-router-dom'
import { NAV_ITEMS, DuplicateBadge, importMenuItems } from '../src/App'
import { scanCommandVisible } from '../src/components/CommandPalette'
import { ScanReportView } from '../src/components/DedupScanDialog'
import {
  JUDGE_TEXT,
  judgeState,
  matchKind,
  needsSecondConfirm,
  rangeLabel,
  rangeNotice,
  resolveLabel,
} from '../src/pages/Dedup'
import type { Capabilities, DupScanReport } from '../src/lib/api'

// i18n-C（卡 t_06629257）：本文件断言的是**中文界面原文**，而迁移后文案走 t()、
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

function caps(over: Partial<Capabilities['features']> = {}, isDesktop = false): Capabilities {
  return {
    isDesktop,
    storeMode: isDesktop ? 'local-disk' : 'server-volume',
    writableRoots: isDesktop ? ['/data'] : [],
    roots: [{ path: '/data', writable: isDesktop, device: 1 }],
    features: {
      moveAsset: isDesktop,
      renameAsset: isDesktop,
      relink: isDesktop,
      openInOS: isDesktop,
      physicalDelete: isDesktop,
      copyInImport: true,
      scanExternalPath: isDesktop,
      ...over,
    },
    featureReasons: isDesktop
      ? {}
      : {
          scanExternalPath:
            '网页版无法读取你电脑上的文件夹（文件在服务器上）。请在桌面版扫描库外文件夹查重。',
        },
  }
}

describe('KI-7 入口收敛：导航 / 徽标 / 扫描入口', () => {
  it('主导航没有「去重」项，图库/搜索/打标仍在（去重页面靠徽标与深链进入）', () => {
    const labels = NAV_ITEMS.map((n) => n.label)
    expect(labels).not.toContain('去重')
    expect(NAV_ITEMS.map((n) => n.to)).not.toContain('/dedup')
    // KI-35 P1：第二档能力不再硬编码进主导航（检查/处理两个动词 tab 已撤），
    // 能力区由已装包注册表驱动（见 tests/capabilityNav.*）—— 静态表只剩区域与内置功能
    for (const keep of ['图库', '搜索', '打标任务', '设置']) {
      expect(labels).toContain(keep)
    }
    for (const gone of ['检查', '处理']) {
      expect(labels).not.toContain(gone)
    }
    expect(NAV_ITEMS.map((n) => n.to)).not.toContain('/compliance')
    expect(NAV_ITEMS.map((n) => n.to)).not.toContain('/process')
  })

  it('侧栏徽标：0 组不渲染，>0 渲染「重复 N 组」并指向 /dedup?range=all', () => {
    const zero = renderToStaticMarkup(
      <MemoryRouter>
        <DuplicateBadge count={0} />
      </MemoryRouter>,
    )
    expect(zero).toBe('')
    const html = renderToStaticMarkup(
      <MemoryRouter>
        <DuplicateBadge count={12} />
      </MemoryRouter>,
    )
    expect(html).toContain('data-testid="sidebar-dups"')
    expect(html).toContain('重复 12 组')
    expect(html).toContain('href="/dedup?range=all"')
  })

  it('「仅检查重复（不入库）」按能力位出现：容器形态隐藏、桌面形态出现', () => {
    // 容器形态（服务端的「本地路径」是服务器磁盘）：只剩导入，不出现扫描项
    const web = importMenuItems(caps({}, false))
    expect(web.map((i) => i.id)).toEqual(['import'])
    expect(web.map((i) => i.label)).not.toContain('仅检查重复（不入库）')
    // 桌面形态：导入 + 仅检查重复（不入库）
    const desktop = importMenuItems(caps({}, true))
    expect(desktop.map((i) => i.id)).toEqual(['import', 'dedup-scan'])
    expect(desktop.map((i) => i.label)).toContain('仅检查重复（不入库）')
    // 命令面板同源门控
    expect(scanCommandVisible(caps({}, false))).toBe(false)
    expect(scanCommandVisible(caps({}, true))).toBe(true)
  })
})

describe('KI-2 处置分级：变体组不许「一键全处理」', () => {
  it('variant 必须二次确认；exact / near / near_visual 走软删 + 撤销窗口', () => {
    expect(needsSecondConfirm('variant')).toBe(true)
    for (const k of ['exact', 'near', 'near_visual'] as const) {
      expect(needsSecondConfirm(k)).toBe(false)
    }
  })

  it('主按钮文案 = 动词 + 真实数量，且不出现「全部」', () => {
    expect(resolveLabel('variant', 3)).toBe('保留这张，其余 3 张移入回收站')
    expect(resolveLabel('exact', 1)).toBe('保留选中，其余 1 张移入回收站')
    expect(resolveLabel('near_visual', 7)).toContain('7 张')
    for (const k of ['exact', 'near', 'near_visual', 'variant'] as const) {
      expect(resolveLabel(k, 5)).not.toContain('全部')
    }
  })

  it('「近似」筛选项包含视觉层（near + near_visual），「变体」单独一档', () => {
    expect(matchKind('near', 'near')).toBe(true)
    expect(matchKind('near_visual', 'near')).toBe(true)
    expect(matchKind('variant', 'near')).toBe(false)
    expect(matchKind('variant', 'variant')).toBe(true)
    expect(matchKind('exact', 'near')).toBe(false)
    expect(matchKind('variant', 'all')).toBe(true)
  })
})

describe('KI-2 三态徽标：找不到向量时不得渲染成「无重复」', () => {
  it('vectors=0 → 未检测；补判未走完 → 检测中；走完 → 已确认；无 judge → 未知', () => {
    const base = {
      model: 'dinov3',
      vectors: 0,
      edges: 0,
      backfillDone: false,
      backfillTruncated: false,
      lastJudgeAt: null,
    }
    expect(judgeState(undefined)).toBe('unknown')
    expect(judgeState(base)).toBe('unjudged')
    expect(judgeState({ ...base, vectors: 10, backfillDone: false })).toBe('running')
    expect(judgeState({ ...base, vectors: 10, backfillDone: true, backfillTruncated: true })).toBe(
      'running',
    )
    expect(judgeState({ ...base, vectors: 10, backfillDone: true })).toBe('confirmed')
    // 未检测的说明里必须点明「不代表没有重复」
    expect(JUDGE_TEXT.unjudged.hint).toContain('不代表')
    expect(JUDGE_TEXT.unjudged.label).toBe('未检测')
  })
})

describe('KI-2 范围（URL 单一真源）', () => {
  it('范围标签可读：全库 / 文件夹 / 选择集 N 张 / 批次 #id', () => {
    expect(rangeLabel('')).toBe('全库')
    expect(rangeLabel('all')).toBe('全库')
    expect(rangeLabel('dir:/test-fixture/a')).toBe('文件夹 /test-fixture/a')
    expect(rangeLabel('ids:3,1,2')).toBe('当前选择集（3 张）')
    expect(rangeLabel('batch:7')).toBe('导入批次 #7')
  })

  it('范围外还有重复时必须显式提示（不许当成「没有重复」）', () => {
    const meta = {
      kind: 'ids' as const,
      prefix: null,
      outOfScopeAssets: 0,
      outsideDupAssets: 2,
    }
    const note = rangeNotice(meta)
    expect(note).not.toBeNull()
    expect(note!).toContain('2 张')
    expect(note!).toContain('不在本次选择内')
    // 全库范围不提示（没有范围外这回事）
    expect(rangeNotice({ kind: 'all', outOfScopeAssets: 0, outsideDupAssets: 0 })).toBeNull()
    // 文件夹范围报出范围外成员数
    expect(
      rangeNotice({ kind: 'dir', prefix: '/d', outOfScopeAssets: 4, outsideDupAssets: 0 }),
    ).toContain('4 张')
  })
})

describe('KI-7 库外扫描报告（只读结果）', () => {
  const report: DupScanReport = {
    scanId: 'scan-abc',
    path: '/tmp/ext',
    scanned: 4,
    truncated: false,
    pendingFiles: 0,
    unreadable: [],
    totalGroups: 1,
    reclaimBytes: 2048,
    kindCounts: { exact: 1 },
    groups: [
      {
        groupId: 1,
        kind: 'exact',
        size: 2,
        reclaimBytes: 2048,
        evidence: ['精确重复（SHA-256 相同）'],
        members: [
          {
            path: '/tmp/ext/a1.png',
            fileName: 'a1.png',
            fileSz: 2048,
            fileHash: 'aa',
            pHash: '00',
            keep: true,
            relation: null,
          },
          {
            path: '/tmp/ext/a2.png',
            fileName: 'a2.png',
            fileSz: 2048,
            fileHash: 'aa',
            pHash: '00',
            keep: false,
            relation: '无变换（汉明距离 0）',
          },
        ],
      },
    ],
    elapsedMs: 12,
    generatedAt: 1700000000,
    algorithm: 'SHA-256 精确重复 ∪ pHash-D4 八变体（汉明距离）近似重复',
    thresholds: 'pHash 汉明 ≤8',
    note: '只读扫描：不入库、不写向量；结果只用于查看与导出',
  }

  it('报告标出保留/重复、组类、证据与「不入库」说明；不出现模型名（KI-6）', () => {
    const html = renderToStaticMarkup(<ScanReportView report={report} />)
    expect(html).toContain('data-testid="scan-report"')
    expect(html).toContain('精确重复（同文件）')
    expect(html).toContain('a1.png')
    expect(html).toContain('a2.png')
    expect(html).toContain('保留')
    expect(html).toContain('重复')
    expect(html).toContain('只读扫描：不入库')
    expect(html).toContain('精确重复（SHA-256 相同）')
    // KI-6：界面文案不出现判重塔模型名/向量字样
    expect(html.toLowerCase()).not.toContain('dinov3')
    expect(html).not.toContain('嵌入')
  })
})

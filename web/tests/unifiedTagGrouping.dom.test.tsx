// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/gallery"}
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { BrowserRouter } from 'react-router-dom'
import { ToastProvider } from '../src/lib/toast'
import {
  groupByTagPrefix,
  parseTagResultValues,
  splitTagKeyValue,
  TAG_PROMPT_FORMAT_CLAUSE,
  promptDeclaresOutputFormat,
} from '../src/lib/tagFormat'
import type { TagRecord, TagResultRecord } from '../src/lib/api'

/**
 * 统一标签裁决（2026-10-19）· key-val 分组展示 + 打标结果区（卡 t_61ae58ed）：
 *  - 值形如 `尺寸-滚筒_10KG` 按第一个 `-` 前缀分组：组标题 = 前缀（任务名），组内 chip = 值；
 *  - 无 `-` 的裸值照旧平铺；分组只是展示层 —— 点击过滤/删除/拖放仍用完整 value 精确匹配；
 *  - Inspector 打标结果区：维度名 + 值；数组快照 `["a","b"]` 逐项 chip；无结果不显示。
 */

// ---------- 纯函数 ----------

describe('tagFormat 纯函数', () => {
  it('splitTagKeyValue：第一个 - 前为组、其余为值；无 - / 前导 - 为裸值', () => {
    expect(splitTagKeyValue('尺寸-滚筒_10KG')).toEqual({ key: '尺寸', val: '滚筒_10KG' })
    expect(splitTagKeyValue('风格-极简')).toEqual({ key: '风格', val: '极简' })
    expect(splitTagKeyValue('青岛')).toEqual({ key: '', val: '青岛' })
    expect(splitTagKeyValue('-怪值')).toEqual({ key: '', val: '-怪值' })
    // 只按第一个 - 拆：值内部可以再有 - 之外的层级，不二次拆
    expect(splitTagKeyValue('尺寸-10-KG')).toEqual({ key: '尺寸', val: '10-KG' })
  })

  it('groupByTagPrefix：裸值组排最前，其余按首次出现顺序；value 保留完整值', () => {
    const groups = groupByTagPrefix(['尺寸-滚筒_10KG', '风光', '尺寸-波轮_8KG', '风格-极简', '青岛'], (v) => v)
    expect(groups.map((g) => g.key)).toEqual(['', '尺寸', '风格'])
    expect(groups[0].entries.map((e) => e.display)).toEqual(['风光', '青岛'])
    expect(groups[1].entries.map((e) => e.value)).toEqual(['尺寸-滚筒_10KG', '尺寸-波轮_8KG'])
    expect(groups[1].entries.map((e) => e.display)).toEqual(['滚筒_10KG', '波轮_8KG'])
  })

  it('parseTagResultValues：JSON 字符串数组逐项返回；非数组/坏 JSON/混类型按原文（null）', () => {
    expect(parseTagResultValues('["青岛","崂山"]')).toEqual(['青岛', '崂山'])
    expect(parseTagResultValues(' [] ')).toEqual([])
    expect(parseTagResultValues('滚筒洗衣机')).toBeNull()
    expect(parseTagResultValues('["a",123]')).toBeNull()
    expect(parseTagResultValues('[broken')).toBeNull()
    expect(parseTagResultValues('{"a":1}')).toBeNull()
  })

  it('promptDeclaresOutputFormat：数组 + （_ 或 下划线）才算声明', () => {
    expect(promptDeclaresOutputFormat(TAG_PROMPT_FORMAT_CLAUSE)).toBe(true)
    expect(promptDeclaresOutputFormat('输出数组，多词用下划线连接')).toBe(true)
    expect(promptDeclaresOutputFormat('输出数组')).toBe(false)
    expect(promptDeclaresOutputFormat('用 _ 连接')).toBe(false)
    expect(promptDeclaresOutputFormat('')).toBe(false)
  })
})

// ---------- Inspector 展示层 ----------

const apiStub = vi.hoisted(() => ({
  assetTags: vi.fn(),
  assetTagResults: vi.fn(),
  ocrAssetText: vi.fn(),
  complianceVerdicts: vi.fn(),
  complianceReview: vi.fn(),
  capabilities: vi.fn(),
  thumbUrl: (id: number, size = 256) => `/api/media/thumb/${id}?size=${size}`,
  originalUrl: (id: number) => `/api/media/original/${id}`,
}))

vi.mock('../src/lib/api', () => ({
  api: apiStub,
  formatSize: (n: number) => `${n} B`,
}))

function tagRec(id: number, value: string): TagRecord {
  return { id, assetId: 11, value, origin: 'custom', model: 'm', taskId: 1, score: 0, createdAt: 0 }
}

function resultRec(id: number, name: string, value: string): TagResultRecord {
  return { id, assetId: 11, taskId: 1, name, value, model: 'm', score: 0, createdAt: 0, updatedAt: 0 }
}

const asset = {
  assetId: 11,
  mediaType: 'image',
  absPath: '/library/a.jpg',
  storeMode: 'index',
  missing: 0,
  fileName: 'a.jpg',
  fileExt: 'jpg',
  fileSz: 2048,
  mtime: 0,
  fileHash: null,
  titleCh: '图',
  titleEn: null,
  description: null,
  source: 'index',
  parentAssetId: null,
  searchable: 1,
  deleted: 0,
  approvalStatus: 'pending',
  starRating: 0,
  colorLabel: null,
  cloudAssetId: null,
  importedAt: 0,
  embStatus: 'done',
  embModel: 'stub',
  dedupStatus: 'none',
  dedupModel: null,
  thumbPath: null,
  width: 1200,
  height: 800,
  pHash: null,
  takenAt: null,
  cameraMake: null,
  cameraModel: null,
  lensModel: null,
  iso: null,
  focalLenMm: null,
  aperture: null,
  shutterText: null,
  orientation: null,
  aspectRatio: null,
  aspectClass: null,
  gpsLat: null,
  gpsLon: null,
  gpsSource: null,
  exif: null,
} as never

const actions = {
  rate: vi.fn(),
  reject: vi.fn(),
  addTags: vi.fn(),
  removeTag: vi.fn(),
  setColor: vi.fn(),
  moveTo: vi.fn(),
  findSimilar: vi.fn(),
}

const { Inspector } = await import('../src/components/Inspector')

function renderInspector() {
  return render(
    <BrowserRouter>
      <ToastProvider>
        <Inspector
          asset={asset}
          batch={[]}
          dirs={[]}
          busy={false}
          similarBusy={false}
          tagsVersion={0}
          actions={actions}
          onPreview={vi.fn()}
        />
      </ToastProvider>
    </BrowserRouter>,
  )
}

beforeEach(() => {
  apiStub.assetTags.mockReset().mockImplementation(async () => [
    tagRec(1, '尺寸-滚筒_10KG'),
    tagRec(2, '尺寸-波轮_8KG'),
    tagRec(3, '青岛'),
  ])
  apiStub.assetTagResults.mockReset().mockImplementation(async () => [
    resultRec(1, '拍摄地点', '["青岛", "崂山"]'),
    resultRec(2, '备注', '客户样机实拍'),
  ])
  apiStub.ocrAssetText.mockReset().mockImplementation(async () => ({ ocrText: '' }))
  apiStub.complianceVerdicts.mockReset().mockImplementation(async (assetId: number) => ({
    packId: 'stub',
    assetId,
    state: 'unknown',
    stateLabel: '未判定',
    machineState: 'unknown',
    machineStateLabel: '未判定',
    machineConfidence: null,
    assetHumanReview: null,
    items: [],
  }))
  apiStub.complianceReview.mockReset().mockImplementation(async () => ({ items: [], total: 0, nextCursor: null }))
  apiStub.capabilities.mockReset().mockImplementation(async () => ({
    isDesktop: false,
    storeMode: 'server-volume',
    writableRoots: [],
    roots: [],
    features: {},
  }))
})

afterEach(() => {
  cleanup()
})

describe('Inspector 标签区 key-val 分组展示', () => {
  it('前缀分组：组标题=前缀，chip 只显示前缀后的值；删除 testid/回调仍用完整 value', async () => {
    renderInspector()
    const group = await screen.findByTestId('tag-group-尺寸')
    expect(group.textContent).toContain('尺寸')
    expect(group.textContent).toContain('滚筒_10KG')
    expect(group.textContent).toContain('波轮_8KG')
    expect(group.textContent).not.toContain('尺寸-滚筒_10KG')
    // 删除入口仍携带完整 value（筛选/删除按完整值精确匹配）
    expect(screen.getByTestId('tag-remove-尺寸-滚筒_10KG')).toBeTruthy()
    expect(screen.getByTestId('tag-remove-尺寸-波轮_8KG')).toBeTruthy()
    fireEvent.click(screen.getByTestId('tag-remove-尺寸-滚筒_10KG'))
    // 乐观移除走 undo toast，落库回调在窗口期后；这里只断言「点中 full value」由 toast 机制接管，
    // 不在窗口期内直接触发 removeTag（与既有 UI-UNDO 语义一致）
    expect(screen.getByTestId('tag-group-plain').textContent).toContain('青岛')
  })

  it('裸值照旧平铺（无组标题），不与分组 chip 混排', async () => {
    renderInspector()
    const plain = await screen.findByTestId('tag-group-plain')
    expect(plain.textContent).toContain('青岛')
    expect(plain.querySelector('[data-testid^="tag-group-"]')).toBeNull()
  })

  it('打标结果区：维度名 + 数组快照逐项 chip；非数组原文展示', async () => {
    renderInspector()
    const panel = await screen.findByTestId('insp-tag-results')
    expect(panel.textContent).toContain('打标结果')
    expect(panel.textContent).toContain('拍摄地点')
    expect(panel.textContent).toContain('备注')
    const chips = panel.querySelectorAll('[data-testid="insp-tag-result-value"]')
    expect([...chips].map((c) => c.textContent)).toEqual(['青岛', '崂山'])
    expect(panel.textContent).toContain('客户样机实拍')
  })

  it('无打标结果 → 整块不渲染（不显示空区块）', async () => {
    apiStub.assetTagResults.mockImplementation(async () => [])
    renderInspector()
    await screen.findByTestId('tag-group-plain')
    expect(screen.queryByTestId('insp-tag-results')).toBeNull()
  })
})

// ---------- LibraryTree 标签云分组 ----------

const { LibraryTree, defaultFilter } = await import('../src/components/LibraryTree')

describe('LibraryTree 标签云 key-val 分组', () => {
  it('组标题 + 行显示前缀后的值；点击过滤仍带完整 value', async () => {
    const onSelect = vi.fn()
    render(
      <BrowserRouter>
        <LibraryTree
          stats={{
            assets: 10,
            tagged: 8,
            missing: 0,
            broken: 0,
            totalBytes: 0,
            byMediaType: [],
            missingCheckedAt: null,
            missingStale: false,
            missingUnreadable: 0,
          } as never}
          dirs={[]}
          roots={[]}
          tags={[
            { value: '尺寸-滚筒_10KG', count: 2 },
            { value: '尺寸-波轮_8KG', count: 1 },
            { value: '风光', count: 5 },
          ]}
          filter={defaultFilter}
          dragActive={false}
          onSelect={onSelect}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </BrowserRouter>,
    )
    const group = await screen.findByTestId('tag-group-尺寸')
    expect(group.textContent).toContain('尺寸')
    expect(group.textContent).toContain('滚筒_10KG')
    expect(group.textContent).toContain('波轮_8KG')
    // 行 testid = 完整 value（拖放打标 / 既有测试契约不变）
    expect(screen.getByTestId('drop-tag-尺寸-滚筒_10KG')).toBeTruthy()
    expect(screen.getByTestId('drop-tag-风光')).toBeTruthy()
    fireEvent.click(screen.getByTestId('drop-tag-尺寸-滚筒_10KG'))
    await waitFor(() =>
      expect(onSelect).toHaveBeenCalledWith(expect.objectContaining({ tag: '尺寸-滚筒_10KG' })),
    )
  })
})

// @vitest-environment jsdom
// @vitest-environment-options {"url": "http://localhost:3000/"}
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'
import { LibraryTree, defaultFilter } from '../src/components/LibraryTree'
import { __setCapabilitiesForTest, canMoveTo, isWritableTarget } from '../src/lib/caps'
import { setDragPayload } from '../src/lib/dnd'
import type { Capabilities } from '../src/lib/api'

/**
 * R36（卡 t_9379ff78）：Windows 真机能力开关的**路径口径**。
 *
 * BATCH-04 真机缺陷：`caps.norm()` 只去尾 `/`、不认 `\`，于是
 * `'C:\\…\\子 目录'.startsWith('C:\\…\\根' + '/')` 永远为 false → 桌面形态下
 * Windows 库树**每一行子目录**都被灰置、拖不进去（服务端 move 本身是好的）。
 *
 * 夹具取自 `reports/evidence/E2E-BATCH-04/raw-b2.json` 的真实返回（真机路径：
 * 中文 + 空格 + 反斜杠），不做手写合成，避免"合成数据自证"。
 */

/** 真机 /api/capabilities 的 writableRoots（raw-b4pre.json，原样） */
const WIN_ROOTS = [
  'C:\\Users\\Administrator\\AppData\\Roaming\\com.soujpg.dam.desktop',
  'C:\\Users\\Administrator\\dam-b4 中文 目录',
  'C:\\Users\\Administrator\\dam-b4-s1',
]

/** 真机 /api/library/dirs 的目录（raw-b2.json b2_dirs 原样） */
const ROOT_DIR = 'C:\\Users\\Administrator\\dam-b4 中文 目录'
const SUB_DIR = 'C:\\Users\\Administrator\\dam-b4 中文 目录\\子 目录'
/** 真机里确实不在任何可写根内的目录（同盘、同父但不同批次根） */
const OUTSIDE_DIR = 'C:\\Users\\Administrator\\Desktop\\不是库的目录'

function winCaps(): Capabilities {
  return {
    isDesktop: true,
    storeMode: 'local-disk',
    writableRoots: WIN_ROOTS,
    roots: WIN_ROOTS.map((path, i) => ({ path, writable: true, device: 67 + i })),
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
  }
}

function renderTree(onDropMove = () => {}) {
  return render(
    <MemoryRouter>
      <LibraryTree
        stats={null}
        dirs={[
          { dir: ROOT_DIR, count: 2 },
          { dir: SUB_DIR, count: 1 },
          { dir: OUTSIDE_DIR, count: 1 },
        ]}
        roots={WIN_ROOTS}
        tags={[]}
        filter={defaultFilter}
        dragActive={false}
        onSelect={() => {}}
        onRefreshDirs={() => {}}
        onDropMove={onDropMove}
        onDropTag={() => {}}
      />
    </MemoryRouter>,
  )
}

/** 行是否被判定为「不可拖放目标」（Row 只在 dropDisabledReason 时打这三个标） */
function dropBlocked(el: HTMLElement): boolean {
  return (
    el.getAttribute('data-drop-disabled') === '1' ||
    el.getAttribute('aria-disabled') === 'true' ||
    (el.getAttribute('title') ?? '').includes('不在可写库根内')
  )
}

describe('R36 caps × 库树：Windows 反斜杠路径不误判灰置', () => {
  beforeEach(() => __setCapabilitiesForTest(winCaps()))
  afterEach(() => {
    __setCapabilitiesForTest(null)
    cleanup()
  })

  it('纯函数口径：真机中文/空格子目录属于可写根', () => {
    const c = winCaps()
    expect(isWritableTarget(c, SUB_DIR)).toBe(true)
    expect(canMoveTo(c, SUB_DIR)).toBe(true)
    expect(canMoveTo(c, SUB_DIR + '\\第三级')).toBe(true)
    // 同盘同父但不在库根内 → 仍不可移动（不能靠放宽通过）
    expect(canMoveTo(c, OUTSIDE_DIR)).toBe(false)
  })

  it('DOM：子目录行不灰置、可作拖放目标；不在库根内的行才灰置', async () => {
    const moves: number[][] = []
    const targets: string[] = []
    renderTree((ids, dir) => {
      moves.push(ids)
      targets.push(dir)
    })

    const sub = await screen.findByTestId(`drop-dir-${SUB_DIR}`)
    expect(dropBlocked(sub)).toBe(false)

    // 真机口径：子目录行能拖进去 → drop 上报 ids + 目标目录
    setDragPayload([11, 12])
    fireEvent.dragOver(sub, { dataTransfer: { dropEffect: '' } })
    fireEvent.drop(sub, { dataTransfer: {} })
    await waitFor(() => expect(moves).toEqual([[11, 12]]))
    expect(targets).toEqual([SUB_DIR])

    // 反向：不在任何可写根内的目录仍然灰置（reason 是人话，带根列表）
    const out = screen.getByTestId(`drop-dir-${OUTSIDE_DIR}`)
    expect(out.getAttribute('data-drop-disabled')).toBe('1')
    expect(out.getAttribute('title')).toContain('不在可写库根内')
  })

  it('DOM：子目录行点击可导航（筛选该目录）', async () => {
    const picked: string[] = []
    render(
      <MemoryRouter>
        <LibraryTree
          stats={null}
          dirs={[
            { dir: ROOT_DIR, count: 2 },
            { dir: SUB_DIR, count: 1 },
          ]}
          roots={WIN_ROOTS}
          tags={[]}
          filter={defaultFilter}
          dragActive={false}
          onSelect={(f) => picked.push(f.dir ?? '')}
          onRefreshDirs={() => {}}
          onDropMove={() => {}}
          onDropTag={() => {}}
        />
      </MemoryRouter>,
    )
    fireEvent.click(await screen.findByTestId(`drop-dir-${SUB_DIR}`))
    expect(picked).toEqual([SUB_DIR])
  })
})

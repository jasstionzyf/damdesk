import { describe, expect, it } from 'vitest'
import {
  buildTree,
  chipsOf,
  defaultFilter,
  isUnderRoot,
  pathSegs,
} from '../src/components/LibraryTree'
import type { DirEntry } from '../src/lib/api'

// Windows 盘符路径：源码里不出现字面反斜杠（避免转义噪音），先写正斜杠再换成反斜杠
const BS = String.fromCharCode(92)
const W = (p: string) => p.split('/').join(BS)

const PICS = W('D:/pics')
const PICS_2024 = W('D:/pics/2024')
const PICS_2025 = W('D:/pics/2025')
const PICS2 = W('D:/pics2')

/**
 * DESKTOP-DIRS-TREE-1：Windows 下目录树恒空。
 * 真机现场 absPath 形如 D:\pics\2024\a.jpg，旧实现按 '/' 取段/取根 → 单段 → 树空/无层级。
 */
describe('isUnderRoot', () => {
  it('POSIX 与 Windows 都成立', () => {
    expect(isUnderRoot('/test-fixture/pics/2024', '/test-fixture/pics')).toBe(true)
    expect(isUnderRoot('/test-fixture/pics', '/test-fixture/pics')).toBe(true)
    expect(isUnderRoot(PICS_2024, PICS)).toBe(true)
    expect(isUnderRoot(PICS, PICS)).toBe(true)
  })

  it('前缀相近但不是子目录时不算在内', () => {
    expect(isUnderRoot('/test-fixture/pics2', '/test-fixture/pics')).toBe(false)
    expect(isUnderRoot(PICS2, PICS)).toBe(false)
  })
})

describe('pathSegs', () => {
  it('两种分隔符都切段', () => {
    expect(pathSegs('/a/b/c')).toEqual(['a', 'b', 'c'])
    expect(pathSegs(PICS_2024)).toEqual(['D:', 'pics', '2024'])
    expect(pathSegs(W('a/b') + '/c')).toEqual(['a', 'b', 'c'])
  })
})

describe('buildTree', () => {
  const winDirs: DirEntry[] = [
    { dir: PICS, count: 1 },
    { dir: PICS_2024, count: 2 },
    { dir: PICS_2025, count: 3 },
  ]

  it('Windows 路径按批次根算相对标签与层级（不再是单层空树）', () => {
    const tree = buildTree(winDirs, [PICS])
    const byDir = new Map(tree.map((n) => [n.dir, n]))
    expect(byDir.get(PICS_2024)!.rel).toBe('2024')
    expect(byDir.get(PICS_2024)!.depth).toBe(1)
    expect(byDir.get(PICS_2025)!.depth).toBe(1)
    // 根节点自身 rel 为空 → 行标签回退成完整路径
    expect(byDir.get(PICS)!.rel).toBe('')
  })

  it('父目录 count 聚合全部后代（含 Windows 分隔符）', () => {
    const tree = buildTree(winDirs, [PICS])
    const root = tree.find((n) => n.dir === PICS)!
    expect(root.count).toBe(1 + 2 + 3)
  })

  it('批次根带尾分隔符也能匹配（真机 rootPath 形态）', () => {
    const tree = buildTree(winDirs, [PICS + BS])
    const child = tree.find((n) => n.dir === PICS_2024)!
    expect(child.rel).toBe('2024')
  })

  it('POSIX 行为不变', () => {
    const posix: DirEntry[] = [
      { dir: '/test-fixture/pics', count: 1 },
      { dir: '/test-fixture/pics/2024', count: 4 },
    ]
    const tree = buildTree(posix, ['/test-fixture/pics/'])
    const child = tree.find((n) => n.dir === '/test-fixture/pics/2024')!
    expect(child.rel).toBe('2024')
    expect(child.depth).toBe(1)
    const root = tree.find((n) => n.dir === '/test-fixture/pics')!
    expect(root.count).toBe(5)
  })

  it('无批次根时整条路径当标签', () => {
    const tree = buildTree([{ dir: W('D:/misc'), count: 2 }], [])
    expect(tree[0].rel).toBe(W('D:/misc'))
    expect(tree[0].depth).toBe(2)
  })
})

describe('chipsOf 目录芯片', () => {
  it('Windows 路径芯片只显示末段目录名', () => {
    const chips = chipsOf({ ...defaultFilter, dir: PICS_2024 })
    expect(chips).toEqual([{ field: 'dir', label: '目录 2024' }])
  })
})

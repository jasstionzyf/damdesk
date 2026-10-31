/**
 * WEBCAP 前端单测：能力开关的判定口径（`web/src/lib/caps.ts`）。
 *
 * 只测**纯函数**（不发请求）：形态级开关 / 逐目录可写判定 / 路径段边界。
 * 真实取值来自 `GET /api/capabilities`，两形态实测在 `scripts/webcap_acceptance.sh`。
 */
import { describe, expect, it } from 'vitest'
import { can, canMoveTo, isWritableTarget, peekCapabilities, reasonFor, __setCapabilitiesForTest } from '../src/lib/caps'
import type { Capabilities } from '../src/lib/api'

function caps(over: Partial<Capabilities> = {}): Capabilities {
  return {
    isDesktop: false,
    storeMode: 'server-volume',
    writableRoots: ['/data'],
    roots: [
      { path: '/data', writable: true, device: 1 },
      { path: '/corpus', writable: false, code: 'read_only_fs', device: 2 },
    ],
    features: {
      moveAsset: false,
      renameAsset: false,
      relink: false,
      openInOS: false,
      physicalDelete: false,
      copyInImport: true,
    },
    featureReasons: { moveAsset: '网页版不支持在磁盘上移动文件…请在桌面版操作。' },
    ...over,
  }
}

describe('caps: 形态级开关', () => {
  it('未加载（null）时一律按可用处理，避免首帧误灰置', () => {
    expect(can(null, 'moveAsset')).toBe(true)
    expect(canMoveTo(null, '/corpus/x')).toBe(true)
    expect(reasonFor(null, 'moveAsset')).toBe('')
  })

  it('网页形态：磁盘类能力关闭，逻辑类（copy-in）仍可用', () => {
    const c = caps()
    expect(can(c, 'moveAsset')).toBe(false)
    expect(can(c, 'physicalDelete')).toBe(false)
    expect(can(c, 'copyInImport')).toBe(true)
    expect(reasonFor(c, 'moveAsset')).toContain('桌面版')
  })

  it('桌面形态：能力打开，无 reason', () => {
    const c = caps({
      isDesktop: true,
      storeMode: 'local-disk',
      features: {
        moveAsset: true,
        renameAsset: true,
        relink: true,
        openInOS: true,
        physicalDelete: true,
        copyInImport: true,
      },
      featureReasons: {},
    })
    expect(can(c, 'moveAsset')).toBe(true)
    expect(reasonFor(c, 'moveAsset')).toBe('')
  })
})

describe('caps: 逐目录可写判定（按路径段，不做字符串前缀）', () => {
  it('可写根内的目录可拖放', () => {
    const c = caps({ features: { ...caps().features, moveAsset: true } })
    expect(isWritableTarget(c, '/data/lib/a')).toBe(true)
    expect(canMoveTo(c, '/data/lib/a')).toBe(true)
    expect(canMoveTo(c, '/data')).toBe(true)
    expect(canMoveTo(c, '/data/')).toBe(true)
  })

  it('只读根 / 不在任何根内的目录不可拖放', () => {
    const c = caps({ features: { ...caps().features, moveAsset: true } })
    expect(isWritableTarget(c, '/corpus/batch1')).toBe(false)
    expect(canMoveTo(c, '/corpus/batch1')).toBe(false)
    expect(isWritableTarget(c, '/database/x')).toBe(false) // 前缀相同但不同路径段
  })

  it('形态关闭时任何目录都不可移动（即使卷内可写）', () => {
    const c = caps()
    expect(canMoveTo(c, '/data/lib')).toBe(false)
  })
})

// R36：Windows 真机路径分隔符是 `\`，服务端 writableRoots 也是反斜杠 ——
// 不做归一的话子目录永远命中不了 `root + '/'`，库树全行被灰置（BATCH-04 真机实测）。
describe('caps: Windows 反斜杠路径（R36）', () => {
  const win = (roots: string[] = ['C:\\Users\\me\\Pictures']) =>
    caps({
      isDesktop: true,
      storeMode: 'local-disk',
      writableRoots: roots,
      roots: roots.map((path, i) => ({ path, writable: true, device: i + 1 })),
      features: { ...caps().features, moveAsset: true },
    })

  it('反斜杠子目录命中反斜杠根（含中文/空格）', () => {
    const c = win(['C:\\Users\\me\\图片 库'])
    expect(isWritableTarget(c, 'C:\\Users\\me\\图片 库\\批次 1')).toBe(true)
    expect(canMoveTo(c, 'C:\\Users\\me\\图片 库\\批次 1\\子目录')).toBe(true)
    expect(canMoveTo(c, 'C:\\Users\\me\\图片 库')).toBe(true)
    expect(canMoveTo(c, 'C:\\Users\\me\\图片 库\\')).toBe(true)
  })

  it('混用分隔符 / 尾部斜杠也能命中', () => {
    const c = win(['C:\\Users\\me\\Pictures'])
    expect(canMoveTo(c, 'C:/Users/me/Pictures/a')).toBe(true)
    expect(canMoveTo(c, 'C:\\Users/me\\Pictures/a/b')).toBe(true)
    expect(canMoveTo(win(['C:/Users/me/Pictures']), 'C:\\Users\\me\\Pictures\\a')).toBe(true)
  })

  it('盘符大小写不敏感', () => {
    const c = win(['C:\\Users\\me\\Pictures'])
    expect(canMoveTo(c, 'c:\\Users\\me\\Pictures\\a')).toBe(true)
    expect(canMoveTo(win(['c:\\Users\\me']), 'C:\\Users\\me\\a')).toBe(true)
  })

  it('盘根本身（C: / C:\\）也是可写目标，且整盘根覆盖其下所有目录', () => {
    const c = win(['C:'])
    expect(canMoveTo(c, 'C:\\Users\\me\\a')).toBe(true)
    expect(canMoveTo(win(['C:\\']), 'C:\\Users')).toBe(true)
    expect(canMoveTo(win(['C:\\']), 'C:')).toBe(true)
  })

  it('只在反斜杠格式下生效的路径段边界依然成立（C:\\data 不含 C:\\database）', () => {
    const c = win(['C:\\data'])
    expect(canMoveTo(c, 'C:\\data\\lib')).toBe(true)
    expect(canMoveTo(c, 'C:\\database\\x')).toBe(false)
    expect(canMoveTo(c, 'D:\\data\\lib')).toBe(false) // 另一个盘符不是同一根
    expect(canMoveTo(c, 'C:\\data2')).toBe(false)
  })
})

describe('caps: 注入接口（测试夹具）', () => {
  it('__setCapabilitiesForTest 写入后 peek 能读到', () => {
    __setCapabilitiesForTest(caps({ isDesktop: true }))
    expect(peekCapabilities()?.isDesktop).toBe(true)
    __setCapabilitiesForTest(null)
    expect(peekCapabilities()).toBeNull()
  })
})

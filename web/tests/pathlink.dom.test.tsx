// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'

/**
 * KI-9 / KI-10（机制层）回归钉：`<PathLink>` 的三条硬要求 ——
 * 1. 能力位 true：渲染**可点按钮**（不是裸文字）且可复制路径；
 * 2. 能力位 false：不渲染打开按钮（灰按钮能点会被骂），改「复制路径」+ 人话原因；
 * 3. 路径缺失：不渲染、不崩。
 * 另加：服务端 `code` → 人话映射（前端不解析文案、不自带白名单）。
 *
 * 形态能力位走 `caps.ts` 的真实读口（`__setCapabilitiesForTest` 注入），只把 `api.osReveal` 换成桩。
 */

// vi.mock 提升到文件顶部 → 桩必须来自 vi.hoisted（否则 TDZ）
const { revealMock, capsMock } = vi.hoisted(() => ({
  revealMock: vi.fn(async (_path: string, _mode?: string) => ({
    ok: true,
    path: _path,
    mode: (_mode ?? 'reveal') as 'reveal' | 'open',
    label: '在文件夹中显示',
    program: 'xdg-open',
    args: ['/data'],
  })),
  capsMock: vi.fn(async () => null),
}))

vi.mock('../src/lib/api', () => ({
  api: { osReveal: revealMock, capabilities: capsMock },
  formatSize: (n: number) => `${n}B`,
}))

import { PathLink, compactHint, compactLabel, humanRevealError, splitPath } from '../src/components/PathLink'
import { __setCapabilitiesForTest } from '../src/lib/caps'
import type { Capabilities } from '../src/lib/api'

function caps(openInOS: boolean): Capabilities {
  return {
    isDesktop: openInOS,
    storeMode: openInOS ? 'local-disk' : 'server-volume',
    writableRoots: openInOS ? ['/data'] : [],
    roots: [{ path: '/data', writable: openInOS, device: 1 }],
    features: {
      moveAsset: openInOS,
      renameAsset: openInOS,
      relink: openInOS,
      openInOS,
      physicalDelete: openInOS,
      copyInImport: true,
    },
    featureReasons: openInOS
      ? {}
      : { openInOS: '网页版无法在系统文件管理器（Finder/Explorer）中打开——文件在服务器上。' },
  }
}

const FILE = '/test-fixture/lib/2024/a.png'

beforeEach(() => {
  revealMock.mockClear()
  Object.defineProperty(navigator, 'clipboard', {
    value: { writeText: vi.fn(async () => {}) },
    configurable: true,
  })
})

afterEach(() => {
  cleanup()
  __setCapabilitiesForTest(null)
  delete (document as unknown as { execCommand?: unknown }).execCommand
})

describe('PathLink（桌面形态：能力位 true）', () => {
  it('渲染 secondary 级按钮（不是裸文字）→ 点击调 /api/os/reveal', async () => {
    __setCapabilitiesForTest(caps(true))
    render(<PathLink path={FILE} mode="reveal" />)

    const btn = screen.getByTestId('pathlink-open')
    expect(btn.textContent).toBe('在文件夹中显示')
    // §9.8：静止态就得看得出来能点 —— 必须有边框 + 非 ink-dim 字色
    expect(btn.className).toContain('border')
    expect(btn.className).toContain('bg-panel-2')
    expect(btn.className).toContain('text-ink')
    expect(btn.className).not.toContain('text-ink-dim')

    fireEvent.click(btn)
    await waitFor(() => expect(revealMock).toHaveBeenCalledTimes(1))
    expect(revealMock).toHaveBeenCalledWith(FILE, 'reveal')
    // 容器形态独有的「原因」行不该出现
    expect(screen.queryByTestId('pathlink-reason')).toBeNull()
  })

  it('可复制路径（剪贴板真被写入，文案短暂变「已复制」）', async () => {
    __setCapabilitiesForTest(caps(true))
    render(<PathLink path={FILE} mode="open" />)

    const copy = screen.getByTestId('pathlink-copy')
    fireEvent.click(copy)
    await waitFor(() => expect(navigator.clipboard.writeText).toHaveBeenCalledWith(FILE))
    await waitFor(() => expect(copy.textContent).toBe('已复制'))
    // 目录级触点：按钮词必须是「打开文件夹」（两种语义不许混词）
    expect(screen.getByTestId('pathlink-open').textContent).toBe('打开文件夹')
    expect(revealMock).not.toHaveBeenCalled()
  })

  it('服务端拒绝（path_missing 409）→ 显示人话，不假装成功', async () => {
    __setCapabilitiesForTest(caps(true))
    revealMock.mockRejectedValueOnce(
      Object.assign(new Error('目标不存在或已断开（可移动盘？）：/test-fixture/x.png'), {
        code: 'path_missing',
        status: 409,
      }),
    )
    render(<PathLink path="/test-fixture/x.png" />)

    fireEvent.click(screen.getByTestId('pathlink-open'))
    const err = await screen.findByTestId('pathlink-error')
    expect(err.getAttribute('role')).toBe('alert')
    expect(err.textContent).toContain('目标不存在或已断开')
  })
})

describe('PathLink（容器形态：能力位 false）', () => {
  it('不渲染打开按钮，改「复制路径」+ 人话原因', async () => {
    __setCapabilitiesForTest(caps(false))
    render(<PathLink path={FILE} mode="open" />)

    expect(screen.queryByTestId('pathlink-open')).toBeNull()
    expect(screen.getByTestId('pathlink').getAttribute('data-gate')).toBe('copy')
    expect(screen.getByTestId('pathlink-path').textContent).toBe(FILE)
    const reason = screen.getByTestId('pathlink-reason')
    expect(reason.textContent).toContain('系统文件管理器')

    fireEvent.click(screen.getByTestId('pathlink-copy'))
    await waitFor(() => expect(navigator.clipboard.writeText).toHaveBeenCalledWith(FILE))
    expect(revealMock).not.toHaveBeenCalled()
  })
})

describe('PathLink 健壮性 / 文案映射', () => {
  it('DT2 剪贴板被拒 → 兜底 execCommand 仍能复制（老 WebView）', async () => {
    __setCapabilitiesForTest(caps(true))
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: vi.fn(async () => {
        throw new Error('denied')
      }) },
      configurable: true,
    })
    const exec = vi.fn(() => true)
    Object.defineProperty(document, 'execCommand', { value: exec, configurable: true, writable: true })
    render(<PathLink path={FILE} />)
    fireEvent.click(screen.getByTestId('pathlink-copy'))
    await waitFor(() => expect(screen.getByTestId('pathlink-copy').textContent).toBe('已复制'))
    expect(exec).toHaveBeenCalledWith('copy')
  })

  it('DT2 剪贴板与兜底都不可用 → 明说复制失败，不假装成功', async () => {
    __setCapabilitiesForTest(caps(true))
    Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true })
    Object.defineProperty(document, 'execCommand', { value: undefined, configurable: true, writable: true })
    render(<PathLink path={FILE} />)
    fireEvent.click(screen.getByTestId('pathlink-copy'))
    expect((await screen.findByTestId('pathlink-error')).textContent).toContain('复制失败')
  })

  it('DT2 兜底抛异常 → 也走「复制失败」分支', async () => {
    __setCapabilitiesForTest(caps(true))
    Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true })
    Object.defineProperty(document, 'execCommand', {
      value: () => {
        throw new Error('nope')
      },
      configurable: true,
      writable: true,
    })
    render(<PathLink path={FILE} />)
    fireEvent.click(screen.getByTestId('pathlink-copy'))
    expect((await screen.findByTestId('pathlink-error')).textContent).toContain('复制失败')
  })

  it.each([null, undefined, '', '   '])('路径缺失（%s）→ 不渲染、不崩', (p: string | null | undefined) => {
    __setCapabilitiesForTest(caps(true))
    const { container } = render(<PathLink path={p} />)
    expect(container.firstChild).toBeNull()
  })

  it('能力位未加载（null）→ 按可用处理（首帧不闪灰），且不崩', () => {
    __setCapabilitiesForTest(null)
    render(<PathLink path={FILE} />)
    expect(screen.getByTestId('pathlink-open')).toBeTruthy()
  })

  it('code → 人话映射覆盖全部错误码（服务端 code 是唯一依据）', () => {
    expect(humanRevealError({ code: 'path_not_allowed' })).toContain('不在允许打开的范围')
    expect(humanRevealError({ code: 'path_missing' })).toContain('可移动盘')
    expect(humanRevealError({ code: 'bad_path' })).toContain('绝对路径')
    expect(humanRevealError({ code: 'feature_disabled' }, '文件在服务器上')).toBe('文件在服务器上')
    expect(humanRevealError({ code: 'reveal_failed' })).toContain('explorer')
    expect(humanRevealError(null)).toBe('打开失败')
  })
})

/**
 * compact 形态：窄容器（侧栏 240px）里库根不再摊开全路径 —— 主线只放名字（单行截断），
 * 副行弱化到「卷 + 一级父目录」；全路径永远在 `title` 与「复制路径」里。
 * 默认 `full` 必须零行为变更（资产路径的验收口径没动）。
 */
describe('DT2 PathLink compact：单行截断 + 弱化父目录（侧栏库根）', () => {
  it('DT2 默认 full：全路径摊开、break-all、无副行（零行为变更）', () => {
    __setCapabilitiesForTest(caps(true))
    render(<PathLink path={FILE} />)
    const path = screen.getByTestId('pathlink-path')
    expect(screen.getByTestId('pathlink').getAttribute('data-display')).toBe('full')
    expect(path.textContent).toBe(FILE)
    expect(path.className).toContain('break-all')
    expect(path.getAttribute('title')).toBe(FILE)
    expect(screen.queryByTestId('pathlink-hint')).toBeNull()
  })

  it('DT2 POSIX：主线 = basename（text-ink + truncate），副行 = /Volumes/SSD/…', async () => {
    __setCapabilitiesForTest(caps(true))
    const p = '/Volumes/SSD/素材/2024'
    render(<PathLink path={p} mode="open" display="compact" />)

    const link = screen.getByTestId('pathlink')
    expect(link.getAttribute('data-display')).toBe('compact')
    expect(link.getAttribute('data-mode')).toBe('open')

    const name = screen.getByTestId('pathlink-path')
    expect(name.textContent).toBe('2024')
    expect(name.className).toContain('truncate')
    expect(name.className).toContain('text-ink')
    expect(name.className).not.toContain('break-all')
    expect(name.getAttribute('title')).toBe(p) // 全路径始终可见（悬停）

    const hint = screen.getByTestId('pathlink-hint')
    expect(hint.textContent).toBe('/Volumes/SSD/…')
    expect(hint.className).toContain('text-ink-dim')
    expect(hint.getAttribute('title')).toBe(p)

    // 动作行不因形态变化而消失：打开文件夹 + 复制路径都还在，语义还是 mode="open"
    expect(screen.getByTestId('pathlink-open').textContent).toBe('打开文件夹')
    fireEvent.click(screen.getByTestId('pathlink-open'))
    await waitFor(() => expect(revealMock).toHaveBeenCalledWith(p, 'open'))
  })

  it('DT2 Windows：D:\\素材\\2024\\sub → 名 sub + 卷/一级 D:\\素材\\…', () => {
    __setCapabilitiesForTest(caps(true))
    render(<PathLink path={'D:\\素材\\2024\\sub'} mode="open" display="compact" />)
    expect(screen.getByTestId('pathlink-path').textContent).toBe('sub')
    expect(screen.getByTestId('pathlink-hint').textContent).toBe('D:\\素材\\…')
  })

  it('DT2 Windows：层级不够时不硬加省略号（D:\\素材\\2024 → D:\\素材）', () => {
    expect(compactHint('D:\\素材\\2024')).toBe('D:\\素材')
    expect(compactHint('D:\\')).toBe('D:\\')
    expect(compactHint('/')).toBe('/')
    expect(compactHint('/corpus/2024')).toBe('/corpus')
    expect(compactHint('lib/2024')).toBe('lib') // 相对路径：副行只回父目录
    expect(compactHint('lib')).toBe('') // 无父目录且无卷 → 副行不渲染
    expect(splitPath('/a/b')).toEqual({ volume: '/', segs: ['a', 'b'], sep: '/' })
    expect(splitPath('D:\\a\\b')).toEqual({ volume: 'D:', segs: ['a', 'b'], sep: '\\' })
    expect(splitPath('a/b')).toEqual({ volume: '', segs: ['a', 'b'], sep: '/' })
  })

  it('DT2 同列表 basename 重名 → 自动多露一级父目录（cap 两级）', () => {
    expect(compactLabel('/a/2024', ['/a/2024', '/b/2024'])).toBe('a/2024')
    expect(compactLabel('/a/2024', ['/a/2024', '/x/2024'])).toBe('a/2024')
    // 两级也重名 → 继续加一级
    expect(compactLabel('/a/x/2024', ['/a/x/2024', '/b/x/2024'])).toBe('a/x/2024')
    // 只有一段（卷根下的同名库根）→ 无法再加父目录，接受同名（title 兜底）
    expect(compactLabel('/2024', ['/x/2024'])).toBe('2024')
    expect(compactLabel('D:\\2024', ['E:\\2024'])).toBe('2024')
    // 不重名 → 只给名字
    expect(compactLabel('/a/2024', ['/a/2025'])).toBe('2024')
    expect(compactLabel('/a/2024')).toBe('2024')
    // 根路径没有 basename → 原样显示
    expect(compactLabel('/')).toBe('/')
    expect(compactLabel('D:\\')).toBe('D:\\')
    // 空 siblings 项不参与比较
    expect(compactLabel('/a/2024', ['', undefined as unknown as string])).toBe('2024')
  })

  it('DT2 compact + 容器形态：仍看得出路径身份（名/卷父目录），但无打开按钮', () => {
    __setCapabilitiesForTest(caps(false))
    render(<PathLink path="/Volumes/SSD/素材/2024" mode="open" display="compact" />)
    expect(screen.getByTestId('pathlink').getAttribute('data-gate')).toBe('copy')
    expect(screen.getByTestId('pathlink-path').textContent).toBe('2024')
    expect(screen.getByTestId('pathlink-hint').textContent).toBe('/Volumes/SSD/…')
    expect(screen.queryByTestId('pathlink-open')).toBeNull()
    expect(screen.getByTestId('pathlink-copy')).toBeTruthy()
    expect(screen.getByTestId('pathlink-reason').textContent).toContain('系统文件管理器')
  })
})


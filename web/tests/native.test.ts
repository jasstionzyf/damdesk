import { afterEach, describe, expect, it, vi } from 'vitest'
import { isDesktopShell, pickFolder } from '../src/lib/native'

/** Tauri 桌面壳形态：注入 __TAURI_INTERNALS__（容器/浏览器形态没有它）。 */
function stubShell(returnValue: unknown = '/Users/me/Pictures', reject?: Error) {
  const invoke = vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    void cmd
    void args
    if (reject) throw reject
    return returnValue
  })
  vi.stubGlobal('window', { __TAURI_INTERNALS__: { invoke } })
  return invoke
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('isDesktopShell', () => {
  it('无 window（容器 node 侧/SSR）→ false', () => {
    expect(isDesktopShell()).toBe(false)
  })

  it('有 window 但无 __TAURI_INTERNALS__（网页/容器形态）→ false', () => {
    vi.stubGlobal('window', {})
    expect(isDesktopShell()).toBe(false)
  })

  it('__TAURI_INTERNALS__ 缺 invoke（非法注入）→ false', () => {
    vi.stubGlobal('window', { __TAURI_INTERNALS__: {} })
    expect(isDesktopShell()).toBe(false)
  })

  it('有完整 __TAURI_INTERNALS__ → true', () => {
    stubShell()
    expect(isDesktopShell()).toBe(true)
  })
})

describe('pickFolder', () => {
  it('非桌面形态：返回 null 且不 invoke（网页形态回退手填，不能崩）', async () => {
    vi.stubGlobal('window', {})
    await expect(pickFolder()).resolves.toBeNull()
  })

  it('桌面形态：走 plugin:dialog|open 且 directory:true / multiple:false，回填绝对路径', async () => {
    const invoke = stubShell('/Volumes/Photos/2026')
    await expect(pickFolder()).resolves.toBe('/Volumes/Photos/2026')
    expect(invoke).toHaveBeenCalledTimes(1)
    expect(invoke.mock.calls[0][0]).toBe('plugin:dialog|open')
    expect(invoke.mock.calls[0][1]).toEqual({
      options: { directory: true, multiple: false, title: '选择图片目录' },
    })
  })

  it('用户取消（插件返回 null）→ null', async () => {
    stubShell(null)
    await expect(pickFolder()).resolves.toBeNull()
  })

  it('插件报错（权限未给 / 非 Tauri 环境）→ 抛出，由调用方转成界面提示', async () => {
    stubShell(null, new Error('dialog.open not allowed'))
    await expect(pickFolder()).rejects.toThrow('dialog.open not allowed')
  })
})

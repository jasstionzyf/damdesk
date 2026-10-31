// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { useState } from 'react'
import { TextField } from '../src/components/TextField'

/**
 * 基础输入组件 `TextField` 的约定测试。
 *
 * 这些用例守的是**踩过的真机 bug**，不是风格：
 *  1. 拼音组合期字必须可见（旧实现吞 onChange → React 把 DOM 值回滚 → 「打不出字」）；
 *  2. `compositionend` 缺失（切窗口 / 输入法取消）不许锁死，后续输入照常；
 *  3. 父级用 `key` 重建子树后焦点要还回来（图库 `AssetGrid key={loadSig}` 就是这个形态）；
 *  4. 提交口径：防抖窗口 / Enter / 失焦 / Esc 删空。
 */

/** 模拟真实输入法：Chromium 的组合期 input 事件带 isComposing=true（fireEvent.change 不带） */
function composingChange(el: HTMLInputElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set
  setter?.call(el, value)
  el.dispatchEvent(new InputEvent('input', { bubbles: true, isComposing: true, data: value }))
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

afterEach(() => cleanup())

describe('TextField：IME 安全', () => {
  it('组合期拼音必须可见，且不提交', () => {
    const onCommit = vi.fn()
    render(<TextField data-testid="f" value="" commitDelayMs={200} onCommit={onCommit} />)
    const el = screen.getByTestId('f') as HTMLInputElement

    el.focus()
    composingChange(el, 'x')
    composingChange(el, 'xue')

    expect(el.value).toBe('xue') // 显示：拼音可见
    expect(onCommit).not.toHaveBeenCalled() // 提交：延后
  })

  it('combinationEnd 缺失也不锁死：IME 直接提交后，后续普通输入照常生效', async () => {
    const onCommit = vi.fn()
    render(<TextField data-testid="f" value="" commitDelayMs={200} onCommit={onCommit} />)
    const el = screen.getByTestId('f') as HTMLInputElement

    el.focus()
    composingChange(el, 'xue') // 只有起点，没有 compositionend
    fireEvent.change(el, { target: { value: '雪' } }) // IME 直接提交（isComposing=false）

    expect(el.value).toBe('雪')
    await waitFor(() => expect(onCommit).toHaveBeenCalledWith('雪'))

    fireEvent.change(el, { target: { value: '雪x' } })
    expect(el.value).toBe('雪x') // 闩锁若残留，这里会停在 '雪'
    await waitFor(() => expect(onCommit).toHaveBeenLastCalledWith('雪x'))
  })

  it('自己提交回传的 value 不回灌 draft（不冲掉同拍新输入）', async () => {
    function Host() {
      const [v, setV] = useState('')
      return <TextField data-testid="f" value={v} commitDelayMs={100} onCommit={setV} />
    }
    render(<Host />)
    const el = screen.getByTestId('f') as HTMLInputElement
    el.focus()

    fireEvent.change(el, { target: { value: 'ab' } })
    await waitFor(() => expect(el.value).toBe('ab'))
    fireEvent.change(el, { target: { value: 'abc' } }) // 提交落地后立刻继续输入
    expect(el.value).toBe('abc')
  })
})

describe('TextField：焦点与提交口径', () => {
  it('父级用 key 重建子树后，焦点与光标回到输入框', async () => {
    function Host() {
      const [sig, setSig] = useState(0)
      return (
        <div>
          <button data-testid="bump" onClick={() => setSig((n) => n + 1)}>
            rebuild
          </button>
          <div key={sig}>
            <TextField data-testid="f" focusKey="k" value="" />
          </div>
        </div>
      )
    }
    render(<Host />)
    const el = screen.getByTestId('f') as HTMLInputElement
    el.focus()
    fireEvent.change(el, { target: { value: 'abc' } })

    fireEvent.click(screen.getByTestId('bump')) // 整块重建（模拟 AssetGrid key 变化）
    const next = screen.getByTestId('f') as HTMLInputElement
    await waitFor(() => expect(document.activeElement).toBe(next))
    expect(next.value).toBe('abc')
  })

  it('真失焦（用户点到别处）不抢回焦点，并把未提交的字落地', async () => {
    const onCommit = vi.fn()
    render(
      <div>
        <TextField data-testid="f" focusKey="k2" value="" commitDelayMs={5000} onCommit={onCommit} />
        <input data-testid="other" />
      </div>,
    )
    const el = screen.getByTestId('f') as HTMLInputElement
    el.focus()
    fireEvent.change(el, { target: { value: 'abc' } })
    ;(screen.getByTestId('other') as HTMLInputElement).focus()

    await waitFor(() => expect(onCommit).toHaveBeenCalledWith('abc')) // 失焦即落地
    expect(document.activeElement).toBe(screen.getByTestId('other')) // 焦点不被抢回
  })

  it('Enter 立即提交；Esc 清空并失焦；删到空立即提交', async () => {
    const onCommit = vi.fn()
    render(<TextField data-testid="f" value="old" commitDelayMs={5000} onCommit={onCommit} />)
    const el = screen.getByTestId('f') as HTMLInputElement
    el.focus()

    fireEvent.change(el, { target: { value: 'new' } })
    expect(onCommit).not.toHaveBeenCalled()
    fireEvent.keyDown(el, { key: 'Enter' })
    expect(onCommit).toHaveBeenLastCalledWith('new')

    fireEvent.keyDown(el, { key: 'Escape' })
    expect(onCommit).toHaveBeenLastCalledWith('')
    expect(el.value).toBe('')

    fireEvent.change(el, { target: { value: 'x' } })
    fireEvent.change(el, { target: { value: '' } })
    await sleep(0)
    expect(onCommit).toHaveBeenLastCalledWith('')
  })
})

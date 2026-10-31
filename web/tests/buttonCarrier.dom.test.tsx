// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { readFileSync } from 'node:fs'
import { Button, buttonClass } from '../src/components/Button'

/**
 * KI-8（卡 task）回归：按钮可点性载体。
 *
 * 断言的是「静止态（不 hover、不 focus）就有可点信号」这一条硬判据
 * （skill soujpg-ui-patterns §9.8），不是「代码里写了 Button」这种静态证据：
 * - 二级/图标载体：ink-dim/60 描边（去色后对底 ≥3:1）+ panel-2 提亮底 + ink 字；
 * - 禁止 text-ink-dim 当按钮字色（灰 = 次要信息，不是可点信号）；
 * - 主按钮实心 accent；选中态用 accent-soft 底色承载（不只换字色）；
 * - disabled 用 opacity-50 保留形状，不「变文字」；
 * - 全局唯一 focus ring（2px accent + offset 2）；
 * - 32×32 图标热区、height ≥32px 的常规热区。
 */

afterEach(cleanup)

describe('Button 载体：静止态可辨（不看 hover 也能认出能点）', () => {
  it('二级（默认）：可见描边 + 提亮底 + ink 字，且不含 ink-dim 字色', () => {
    render(<Button>导出 (E)</Button>)
    const el = screen.getByRole('button', { name: '导出 (E)' })
    const cls = el.className
    // 静止态三个信号
    expect(cls).toContain('border')
    expect(cls).toContain('border-ink-dim/60')
    expect(cls).toContain('bg-panel-2')
    expect(cls).toContain('text-ink')
    // 反例守卫：灰字不是可点信号
    expect(cls).not.toContain('text-ink-dim')
    // 热区 ≥32px
    expect(cls).toContain('h-8')
  })

  it('主按钮：实心 accent 底 + 白字；md 尺寸高度 ≥32px', () => {
    render(
      <Button variant="primary" size="md">
        导入图片
      </Button>,
    )
    const cls = screen.getByRole('button', { name: '导入图片' }).className
    expect(cls).toContain('bg-accent')
    expect(cls).toContain('text-white')
    expect(cls).toContain('h-9')
  })

  it('图标按钮：32×32 热区 + 同样的描边与底（纯图标必须自报名字）', () => {
    render(
      <Button variant="icon" aria-label="收起检查器" title="收起检查器">
        ›
      </Button>,
    )
    const el = screen.getByRole('button', { name: '收起检查器' })
    expect(el.className).toContain('h-8')
    expect(el.className).toContain('w-8')
    expect(el.className).toContain('border-ink-dim/60')
    expect(el.className).toContain('bg-panel-2')
    expect(el.getAttribute('title')).toBe('收起检查器')
  })

  it('文字链接载体：accent 色 + 下划线（动作不得做成裸文字）', () => {
    const cls = buttonClass({ variant: 'link' })
    expect(cls).toContain('text-accent')
    expect(cls).toContain('underline')
  })

  it('危险动作：红字 + 红描边（删除仍看得出是按钮）', () => {
    render(<Button tone="danger">删除 (Del)</Button>)
    const cls = screen.getByRole('button', { name: '删除 (Del)' }).className
    expect(cls).toContain('text-red-300')
    expect(cls).toContain('border-red-400/70')
    expect(cls).toContain('bg-panel-2')
  })
})

describe('四态：hover / 选中 / 禁用 / 键盘焦点', () => {
  it('选中态用 accent-soft 底色 + aria-pressed，不只换字色', () => {
    render(<Button active>精确</Button>)
    const el = screen.getByRole('button', { name: '精确' })
    expect(el.getAttribute('aria-pressed')).toBe('true')
    expect(el.className).toContain('bg-accent-soft')
    expect(el.className).toContain('border-accent')
  })

  it('普通动作按钮不声明 aria-pressed（语义不误用）', () => {
    render(<Button>清空筛选</Button>)
    expect(screen.getByRole('button', { name: '清空筛选' }).getAttribute('aria-pressed')).toBeNull()
  })

  it('禁用态保留形状（opacity-50）且不可点', () => {
    const onClick = vi.fn()
    render(
      <Button disabled onClick={onClick}>
        拒选 (X)
      </Button>,
    )
    const el = screen.getByRole('button', { name: '拒选 (X)' }) as HTMLButtonElement
    expect(el.disabled).toBe(true)
    expect(el.className).toContain('disabled:opacity-50')
    fireEvent.click(el)
    expect(onClick).not.toHaveBeenCalled()
  })

  it('focus ring 唯一实现：2px accent + offset 2（全仓不再各自 outline-none）', () => {
    render(<Button>并排对比</Button>)
    const cls = screen.getByRole('button', { name: '并排对比' }).className
    expect(cls).toContain('focus-visible:ring-2')
    expect(cls).toContain('focus-visible:ring-accent')
    expect(cls).toContain('focus-visible:ring-offset-2')
  })
})

describe('组件契约', () => {
  it('默认 type="button"（避免误触发表单提交），属性透传', () => {
    const onClick = vi.fn()
    render(
      <Button data-testid="probe" title="提示" onClick={onClick}>
        探测
      </Button>,
    )
    const el = screen.getByTestId('probe') as HTMLButtonElement
    expect(el.getAttribute('type')).toBe('button')
    expect(el.getAttribute('title')).toBe('提示')
    fireEvent.click(el)
    expect(onClick).toHaveBeenCalledTimes(1)
  })

  it('buttonClass() 供 <Link>/<a> 复用同一套静态信号（不写第二套样式）', () => {
    const cls = buttonClass({ size: 'md', className: 'w-full' })
    expect(cls).toContain('border-ink-dim/60')
    expect(cls).toContain('bg-panel-2')
    expect(cls).toContain('text-ink')
    expect(cls).toContain('w-full')
  })
})

describe('回归守卫：本卡三处载体不再手写按钮样式', () => {
  const src = (p: string) => readFileSync(new URL(p, import.meta.url), 'utf8')

  it('三页里「灰字 + 只在 hover 才变化」的按钮字样归零', () => {
    for (const f of ['../src/pages/Gallery.tsx', '../src/components/Inspector.tsx', '../src/pages/Dedup.tsx']) {
      const code = src(f)
      // 旧字样：text-ink-dim 当按钮字色、靠 hover:border-accent 才暴露可点 → 静止态与文本同视觉
      expect(code, `${f} 仍有把灰字当按钮字色`).not.toContain('text-ink-dim hover:border-accent')
      expect(code, `${f} 仍有旧二级按钮 className 手写`).not.toContain('px-2.5 py-1 text-xs text-ink-dim')
    }
  })

  it('原生 <button> 仅剩「图片热区」两处（载体是图片本身，不是文字按钮）', () => {
    // Gallery 的星标/工具栏/把手全部组件化，原生 button 清零；
    // Inspector 缩略图预览热区、Dedup 组内选图热区，载体是整张图片，用 Button 会加出边框与外框。
    const gallery = src('../src/pages/Gallery.tsx')
    const inspector = src('../src/components/Inspector.tsx')
    const dedup = src('../src/pages/Dedup.tsx')
    expect(gallery.split('<button').length - 1).toBe(0)
    expect(inspector.split('<button').length - 1).toBe(1)
    expect(dedup.split('<button').length - 1).toBe(1)
    // 剩下这两处的载体都是整张图片（className 里没有任何按钮外观类）
    expect(inspector).toContain('className="aspect-4/3 w-full overflow-hidden bg-panel-2"')
    expect(dedup).toContain('className="block w-full cursor-pointer bg-panel-2"')
  })
})

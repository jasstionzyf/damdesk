import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { ConfirmDialog, impactLine, verbCount, type ConfirmOptions } from '../src/lib/confirm'

/**
 * Confirm 文案单测（卡 t_2d425292 验收 2 第③条）：
 * 不可逆/高成本操作的确认框里，主按钮必须是「动词 + **真实数量**」，
 * 禁止出现「全部」；默认焦点在取消（DOM 顺序上取消在前，真实 focus 见 e2e 证据）。
 */
describe('ConfirmDialog 文案与焦点顺序', () => {
  it('verbCount：动词 + 真实数量，不出现「全部」', () => {
    expect(verbCount('移动', 3)).toBe('移动 3 张')
    expect(verbCount('移动', 1)).toBe('移动 1 张')
    for (const n of [1, 2, 17, 300]) {
      expect(verbCount('移动', n)).not.toContain('全部')
    }
  })

  it('impactLine：真实条数 → 明细行；0/NaN → null（不渲染「影响 0 项」噪音）', () => {
    expect(impactLine('移动图片', 3)).toBe('移动图片：3 项')
    expect(impactLine('移动图片', 0)).toBeNull()
    expect(impactLine('移动图片', Number.NaN)).toBeNull()
  })

  it('SSR：对话框渲染出真实数量主按钮 + 影响明细 + 可回滚性说明', () => {
    const opts: ConfirmOptions = {
      title: '移动图片文件',
      body: '把选中的图片移动到「batch2/」，磁盘上的文件会真的换目录（库内路径同步更新）。',
      impacts: [impactLine('移动图片', 3)!, '目标目录：/corpus/batch2'],
      recoverable: '不会删除文件；要撤回需再把这些图移回原目录',
      confirmLabel: verbCount('移动', 3),
    }
    const html = renderToStaticMarkup(<ConfirmDialog opts={opts} onCancel={() => {}} onConfirm={() => {}} />)
    console.log('[DOM] confirm =', html)

    expect(html).toContain('data-testid="confirm-dialog"')
    expect(html).toContain('>移动 3 张</button>') // 主按钮 = 动词 + 真实数量
    expect(html).toContain('移动图片：3 项')
    expect(html).toContain('目标目录：/corpus/batch2')
    expect(html).toContain('data-testid="confirm-recoverable"')
    // 弹窗三条出口：Esc（键盘）/ 右上角 ✕ / 取消按钮
    expect(html).toContain('data-testid="confirm-close"')
    expect(html).toContain('aria-label="关闭"')
    expect(html).not.toContain('全部')
    // 默认焦点在取消：DOM 里「取消」按钮先出现（ConfirmDialog 用 ref.focus() 聚焦它）
    expect(html.indexOf('data-testid="confirm-cancel"')).toBeLessThan(
      html.indexOf('data-testid="confirm-ok"'),
    )
  })

  it('danger 变体：主按钮红底（不可撤销语义），仍带数量', () => {
    const html = renderToStaticMarkup(
      <ConfirmDialog
        opts={{
          title: '清空回收站',
          body: '示例：彻底删除将不可恢复',
          confirmLabel: verbCount('彻底删除', 2),
          recoverable: '不可撤销',
          danger: true,
        }}
        onCancel={() => {}}
        onConfirm={() => {}}
      />,
    )
    expect(html).toContain('>彻底删除 2 张</button>')
    expect(html).toContain('bg-red-600')
  })
})

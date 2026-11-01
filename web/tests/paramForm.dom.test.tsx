// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { ParamForm, stripHidden } from '../src/components/ParamForm'
import type { ParamSchemaDoc } from '../src/lib/api'

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

/**
 * N3+N4 前端验收（卡 t_d927e0d4）：`ParamForm` = 插件参数的**唯一**通用渲染器。
 *
 * 守的是控件映射表与三条硬规则（`plugin-param-schema.md` §2/§3）：
 *  T1 8 种 `type` 各有唯一控件（number/integer 有 min+max+step → 滑杆 + 手输；enum ≤8 分段按钮、
 *     >8 下拉；boolean 开关；string 单行；color 取色 + hex；asset_ref 资产引用；range 双值）；
 *  T2 **默认值在插件里，规格只存 diff**：改回默认值 → 该键从 diff 里删掉；
 *  T3 `when` 条件显隐 + 提交前剔除（界面看到的就是将要提交的；宿主还会再校一次）；
 *  T4 分组 + 高级折叠：`collapsed`/`advanced` 默认收起，基础组直出；
 *  T5 `defaults`（只读回显）/ `disabled`（未安装灰态）两态：控件 disabled + 说明。
 */

const SCHEMA: ParamSchemaDoc = {
  schemaVersion: 1,
  groups: [
    { key: 'basic', title: '判定门槛' },
    { key: 'adv', title: '高级', collapsed: true },
  ],
  properties: {
    format: { type: 'enum', title: '输出格式', options: ['jpg', 'png', 'webp'], default: 'webp', group: 'basic' },
    quality: {
      type: 'integer',
      title: '质量',
      min: 1,
      max: 100,
      step: 1,
      default: 82,
      unit: '%',
      group: 'basic',
      when: { format: ['jpg', 'webp'] },
    },
    keepRatio: { type: 'boolean', title: '等比缩放', default: true, group: 'basic' },
    opacity: { type: 'number', title: '不透明度', min: 0, max: 1, step: 0.05, default: 1, group: 'adv' },
    watermark: { type: 'asset_ref', title: '水印文件', accept: ['image/png'], advanced: true },
    note: { type: 'string', title: '备注', default: '', advanced: true },
    tint: { type: 'color', title: '色调', default: '#ffffff', advanced: true },
    crop: { type: 'range', title: '裁切范围', min: 0, max: 100, step: 1, advanced: true },
    pick: { type: 'enum', title: '长枚举', options: ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i'], advanced: true },
  },
  required: ['format'],
}

afterEach(() => cleanup())

describe('ParamForm：控件映射（宿主唯一一套）', () => {
  it('integer 有 min+max+step → 滑杆 + 可手输；改手输只写 diff', () => {
    const onChange = vi.fn()
    render(<ParamForm schema={SCHEMA} value={{}} onChange={onChange} />)
    // 滑杆与手输同时存在（§2 映射表）
    expect(screen.getByTestId('param-range-quality')).toBeTruthy()
    const input = screen.getByTestId('param-input-quality') as HTMLInputElement
    // 默认值预填（默认值来自插件）
    expect(input.value).toBe('82')
    fireEvent.change(input, { target: { value: '70' } })
    expect(onChange).toHaveBeenCalledWith({ quality: 70 })
    // 单位与默认值提示都在
    expect(screen.getByTestId('param-hint-quality').textContent).toContain('默认 82')
    expect(screen.getByTestId('param-hint-quality').textContent).toContain('步长 1')
  })

  it('数量型 enum ≤8 项 → 分段按钮；>8 项 → 下拉', () => {
    render(<ParamForm schema={SCHEMA} value={{}} />)
    expect(screen.getByTestId('param-option-format-webp').getAttribute('aria-pressed')).toBe('true')
    fireEvent.click(screen.getByTestId('param-option-format-png'))
    // 下拉只出现在 >8 项的枚举上
    expect(screen.getByTestId('param-select-pick')).toBeTruthy()
    expect(screen.queryByTestId('param-select-format')).toBeNull()
  })

  it('boolean → 开关；string/color/asset_ref/range 各有专用控件', () => {
    render(<ParamForm schema={SCHEMA} value={{}} />)
    const toggle = screen.getByTestId('param-toggle-keepRatio') as HTMLInputElement
    expect(toggle.checked).toBe(true)
    // 高级组默认收起 → 先展开
    fireEvent.click(screen.getByText(/高级（/))
    expect(screen.getByTestId('param-input-note')).toBeTruthy()
    expect(screen.getByTestId('param-color-tint')).toBeTruthy()
    expect(screen.getByTestId('param-input-watermark').getAttribute('placeholder')).toBe('资产引用 id')
    // range = 双值（两个数字框，不存路径/字符串）
    expect(screen.getByTestId('param-input-crop-0')).toBeTruthy()
    expect(screen.getByTestId('param-input-crop-1')).toBeTruthy()
  })
})

describe('ParamForm：diff 语义与条件显隐', () => {
  it('改回默认值 = 该键从 diff 删除（插件升级默认值时不锁死旧规格）', () => {
    const onChange = vi.fn()
    render(<ParamForm schema={SCHEMA} value={{ quality: 70 }} onChange={onChange} />)
    fireEvent.change(screen.getByTestId('param-input-quality'), { target: { value: '82' } })
    expect(onChange).toHaveBeenCalledWith({})
    // 有改动才有「恢复默认」
    expect(screen.getByTestId('param-reset-quality')).toBeTruthy()
    fireEvent.click(screen.getByTestId('param-reset-quality'))
    expect(onChange).toHaveBeenLastCalledWith({})
  })

  it('when 不成立 → 控件隐藏，且该键从 diff 中剔除（提交的就是看到的）', () => {
    const onChange = vi.fn()
    // 当前 format=png：png 不在 quality 的 when 列表里 → 质量不显示（旧值也不该被静默提交）
    render(<ParamForm schema={SCHEMA} value={{ format: 'webp', quality: 70 }} onChange={onChange} />)
    expect(screen.getByTestId('param-field-quality')).toBeTruthy()
    fireEvent.click(screen.getByTestId('param-option-format-png'))
    // 切到 png：质量这一项不再成立 → onChange 只带 format，quality 被剔除
    expect(onChange).toHaveBeenCalledWith({ format: 'png' })
  })

  it('一开始就不成立的键：控件不渲染（不显示又提交是最坏的组合）', () => {
    render(<ParamForm schema={SCHEMA} value={{ format: 'png', quality: 70 }} />)
    expect(screen.queryByTestId('param-field-quality')).toBeNull()
  })

  it('stripHidden 是纯函数（宿主合并口径一致）', () => {
    expect(stripHidden(SCHEMA, { format: 'png', quality: 70 })).toEqual({ format: 'png' })
    expect(stripHidden(SCHEMA, { format: 'webp', quality: 70 })).toEqual({ format: 'webp', quality: 70 })
    // schema 里没有的键不静默丢（宿主会报 params_unknown_key，错误要可见）
    expect(stripHidden(SCHEMA, { whatever: 1 })).toEqual({ whatever: 1 })
  })

  it('高级/折叠组默认收起，基础组直出', () => {
    const { container } = render(<ParamForm schema={SCHEMA} value={{}} />)
    expect(screen.getByTestId('param-group-basic')).toBeTruthy()
    // adv 组用 <details> 承载：默认没有 open（浏览器里就是收起的），标题写明「默认收起」
    const adv = screen.getByTestId('param-group-adv')
    const details = adv.querySelector('details')
    expect(details).toBeTruthy()
    expect(details!.open).toBe(false)
    expect(screen.getByText(/高级（\d+ 项，默认收起）/)).toBeTruthy()
    fireEvent.click(screen.getByText(/高级（\d+ 项，默认收起）/))
    expect((container.querySelector('details') as HTMLDetailsElement).open).toBe(true)
  })
})

describe('ParamForm：只读两态（默认设置 / 未安装灰态）', () => {
  it('defaults：控件全 disabled + 说明「只读：控件灰着不能改」', () => {
    render(<ParamForm schema={SCHEMA} mode="defaults" />)
    expect(screen.getByTestId('param-defaults-note').textContent).toContain('只读')
    expect((screen.getByTestId('param-input-quality') as HTMLInputElement).disabled).toBe(true)
    expect((screen.getByTestId('param-toggle-keepRatio') as HTMLInputElement).disabled).toBe(true)
  })

  it('disabled：未安装 → 灰态 + 「装好后可用」，声明照样看得见', () => {
    render(<ParamForm schema={SCHEMA} mode="disabled" />)
    expect(screen.getByTestId('param-disabled-note').textContent).toContain('装好后可用')
    expect((screen.getByTestId('param-input-quality') as HTMLInputElement).disabled).toBe(true)
    // 灰态不是空面板：控件仍在
    expect(screen.getByTestId('param-option-format-webp')).toBeTruthy()
  })

  it('老包（没声明参数）→ 空面板说明，不画控件', () => {
    render(<ParamForm schema={{ properties: {} }} />)
    expect(screen.getByTestId('param-empty').textContent).toContain('没有可配置参数')
  })
})

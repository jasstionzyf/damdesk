/**
 * N12 文案守卫（L0 里跑，不靠人记得手动 grep）。
 *
 * 设计口径（`plugin-design-and-ui.md` §3）：用户视角只有两动词三名词；界面禁用词
 * 插件 / job / verdict / 行业包。守卫脚本 `scripts/ui_copy_guard.py` 只扫**用户可见文案**
 * （JSX 文本 + 含中文的字符串字面量；技术标识如 data-testid 不算），
 * 唯一例外是设置里的「插件与方案目录」。
 *
 * 这里把脚本本身也纳入门禁：脚本失效（退出码 2，扫描目录不存在）同样算红。
 */
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'

import { describe, expect, it } from 'vitest'

const ROOT = resolve(__dirname, '../..')

describe('N12 文案守卫：用户可见文案 0 命中禁用词', () => {
  it('scripts/ui_copy_guard.py 退出码 0（行业包 / 插件 / job / verdict）', () => {
    const r = spawnSync('python3', [resolve(ROOT, 'scripts/ui_copy_guard.py')], { encoding: 'utf-8' })
    expect(r.status, `${r.stdout ?? ''}${r.stderr ?? ''}`).toBe(0)
    expect(r.stdout).toContain('0 命中')
  })
})

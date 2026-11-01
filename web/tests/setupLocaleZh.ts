/**
 * 全局测试环境设定（i18n-D，卡 t_4eea3e75）。
 *
 * 背景：i18n 基座（i18n-A）首启判定 = localStorage `dam.locale` → navigator.language
 * 前缀 zh* → zh-Hans，否则 en-US。jsdom 的 navigator.language 默认 'en-US'，node 环境
 * 连 localStorage/navigator 都没有（同样落到 en-US）——i18n-D 迁移后 en 默认会让全部
 * 存量用例看到英文文案，中文断言整体翻红（实测 32 文件 90 例）。
 *
 * 处理：测试环境统一钉 zh-Hans（= 存量用例书写时的产品默认语言，断言零改动、强度不降）；
 * en-US 行为由专门的新用例覆盖（i18nSettingsEn.dom.test.tsx / apiErrorI18n.test.ts）。
 * A 卡 i18n.test.tsx 的 navigator.language 判定 4 例自带局部覆盖（defineProperty 后还原），
 * 与本 setup 不冲突。
 *
 * 时机：vitest setupFiles 在**每个测试文件的模块求值前**执行 —— i18n 模块的模块级
 * globalLocale 在 import 时调 resolveInitialLocale()，此时已能读到这里的钉值。
 */
if (typeof window !== 'undefined') {
  // jsdom：钉 navigator.language（真实 localStorage 留着，i18n.test.tsx 自己换值测判定）
  Object.defineProperty(window.navigator, 'language', { value: 'zh-CN', configurable: true })
} else {
  // node：无 localStorage，i18n 走 navigator 判定 → en-US。给最小 stub，**只**回
  // `dam.locale=zh-Hans`、其余 key 一律 null（等价于今天「没有 localStorage」的行为，
  // 不影响 searchResults/urlState 这类自带 stub 或走缺省路径的用例）。
  ;(globalThis as unknown as { localStorage: Storage }).localStorage = {
    getItem: (k: string) => (k === 'dam.locale' ? 'zh-Hans' : null),
    setItem: () => {},
    removeItem: () => {},
    clear: () => {},
  } as unknown as Storage
}

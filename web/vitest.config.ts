import { defineConfig } from 'vitest/config'

// W7：web bundle 的单测（纯函数逻辑，node 环境，无 DOM）。旧桌面前端 vitest
// （jsdom + invoke mock）随 invoke 层一并退役，历史见 git。
export default defineConfig({
  // cacheDir 指到仓内可写目录：容器构建出来的 node_modules 属 root，默认缓存目录写不进去
  // （整套测试跑完后在收尾处 EACCES，看起来像测试失败）。
  cacheDir: '.vitest-cache',
  test: {
    environment: 'node',
    include: ['tests/**/*.test.{ts,tsx}'],
    // 语言钉桩：默认 zh-Hans（产品默认语言）。B 批（task）与 D 批（task）
    // 各加过一份同样的桩，i18n-G 合流合并为一份（tests/setupLocaleZh.ts，
    // 覆盖 jsdom 的 navigator 与 node 的 localStorage 两种情况）；en 行为由
    // i18nSettingsEn / i18nGalleryEn / i18nResultsEn / apiErrorI18n 等专测覆盖。
    setupFiles: ['./tests/setupLocaleZh.ts'],
    // 手验档（真实后端，需要先起 app-web）单独跑：`vitest run --config vitest.live.config.ts`
    exclude: ['**/node_modules/**', 'tests/live/**'],
  },
})

import { defineConfig } from 'vitest/config'

/**
 * 「手验」档（不进 L0 门禁）：跑**真实后端**的插件详情页链路。
 *
 * 为什么单独一份 config：`vitest.config.ts` 的用例全是打桩的（`api` 被 mock），
 * 而这一档要求 `fetch` 打到真的 `crates/app-web`（`DAM_WEB_BASE`，默认 127.0.0.1:8791），
 * 装插件 → 建规格 → 先跑 1 张 都是真接口、真落库。混进门禁会拖慢并依赖外部进程。
 *
 * 用法（容器内）：
 *   /cargo-target/debug/app-web 先起（DAM_WEB_PORT=8791 DAM_WEB_DATA=/tmp/dam-n4-data DAM_COMPLIANCE_ENGINE=plugin）
 *   pnpm -C web exec vitest run --config vitest.live.config.ts
 */
export default defineConfig({
  cacheDir: '.vitest-cache',
  test: {
    environment: 'jsdom',
    include: ['tests/live/**/*.live.test.tsx'],
    testTimeout: 60000,
    hookTimeout: 30000,
  },
})

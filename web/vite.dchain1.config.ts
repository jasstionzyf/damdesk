// D-CHAIN-1 自检用配置：把依赖预构建缓存挪到可写目录（node_modules 是共享只读目录），
// 其余完全继承 web/vite.config.ts（端口/代理口径一致，避免「测的不是同一套配置」）。
import { defineConfig, mergeConfig } from 'vite'
import base from './vite.config'

export default defineConfig((env) =>
  mergeConfig(base(env) as never, { cacheDir: '/tmp/dchain1-vite-cache' } as never),
)

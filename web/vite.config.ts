import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// dev: vite 直连后端 —— /api 反代到 app-web（cargo run -p app-web，默认 127.0.0.1:8787；
// 可用环境变量 VITE_API_TARGET / VITE_DEV_PORT 覆盖，便于同机多套 dev 栈并行，
// 如 E3 隔离栈：VITE_API_TARGET=http://127.0.0.1:8797 VITE_DEV_PORT=5187）
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, '.', '')
  const apiTarget = env.VITE_API_TARGET || 'http://127.0.0.1:8787'
  const devPort = Number(env.VITE_DEV_PORT || 5180)
  return {
    plugins: [react(), tailwindcss()],
    server: {
      port: devPort,
      proxy: {
        '/api': {
          target: apiTarget,
          changeOrigin: true,
        },
      },
    },
  }
})

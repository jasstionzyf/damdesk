# soujpg-dam-desktop

SOUJPG DAM Desktop - open-source local-first DAM (Tauri 2 + React + SQLite + LanceDB), powered by dam-ai inference
## dam-web（W0 起）

浏览器版（web 先行）：`crates/app-web`（axum）复用全部业务 crates，`web/` 为前端（Vite + React 18 + Tailwind v4，暗色默认）。

开发（dam-dev 容器内，见 PLAN.md §11）：

```bash
pnpm -C web install
pnpm -C web dev          # vite :5180，/api 反代 127.0.0.1:8787
cargo run -p app-web     # 监听 127.0.0.1:${DAM_WEB_PORT:-8787}，数据在 ${DAM_WEB_DATA:-./data}
```

生产（容器部署铁律）：

```bash
docker compose -f deploy/docker-compose.yml up -d --build
curl 127.0.0.1:8787/healthz
```

前端产物 serve 自 `web/dist`（env `DAM_WEB_DIST` 可覆盖）；容器内绑 `0.0.0.0`，宿主端口只发 `127.0.0.1:8787`（R4 单用户）。

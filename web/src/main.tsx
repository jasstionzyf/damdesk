import React from 'react'
import ReactDOM from 'react-dom/client'
import { createBrowserRouter, Navigate, RouterProvider } from 'react-router-dom'
import App, { ExportRedirect, PluginRouteRedirect } from './App'
import { Gallery } from './pages/Gallery'
import { SmartFolderRedirect } from './pages/SmartFolderRedirect'
import { Search } from './pages/Search'
import { Settings } from './pages/Settings'
import { TagTasks } from './pages/TagTasks'
import { TagReview } from './pages/TagReview'
import { Dedup } from './pages/Dedup'
import { TagBrowser } from './pages/TagBrowser'
import { PluginDetail } from './pages/PluginDetail'
import { Process } from './pages/Process'
import { Tasks } from './pages/Tasks'
import { Trash } from './pages/Trash'
import { ToastProvider } from './lib/toast'
import { ConfirmProvider } from './lib/confirm'
import { TaskCenterProvider } from './lib/tasks'
import { LocaleProvider } from './i18n'
import { loadCapabilities } from './lib/caps'
import { BUILTIN_JUDGE_PLUGIN_ID, capabilityPath } from './lib/capabilityNav'
import './index.css'

// WEBCAP：启动即拉一次形态能力开关（模块级缓存，整个会话一次请求）。
// 失败不阻塞启动（老后端没有该端点时 UI 按「可用」处理，不误灰置）。
void loadCapabilities().catch(() => {})

const router = createBrowserRouter([
  {
    path: '/',
    element: <App />,
    children: [
      { index: true, element: <GalleryRoute /> },
      // KI-16 #7：智能文件夹不再是独立浏览面 —— 老深链只做重定向（→ /?<条件>&sf=<id>）
      { path: 'smart/:id', element: <SmartFolderRedirect /> },
      { path: 'search', element: <Search /> },
      {
        path: 'dedup',
        element: <Dedup />,
      },
      {
        path: 'tag-tasks',
        element: <TagTasks />,
      },
      // task：标签总览（低频但必要时要用 —— 不进 NAV，入口=侧栏「标签→全部」/命令面板）
      {
        path: 'tags',
        element: <TagBrowser />,
      },
      {
        path: 'tag-review',
        element: <TagReview />,
      },
      // KI-35 P1（task）：能力详情页路由统一为 `/processing/:pluginId`
      // （路由约定见 lib/capabilityNav.capabilityPath，P2 三段详情页 / P3 选中集入口沿用）。
      // 旧深链兼容：/plugins/:pluginId（N4）与 /compliance（N12 检查页）都重定向过来 ——
      // /compliance 落到产品唯一内置的判定能力（电商合规）的详情页。
      {
        path: 'processing/:pluginId',
        element: <PluginDetail />,
      },
      {
        path: 'plugins/:pluginId',
        element: <PluginRouteRedirect />,
      },
      {
        path: 'compliance',
        element: <Navigate to={capabilityPath(BUILTIN_JUDGE_PLUGIN_ID)} replace />,
      },
      // DECouple-FE（卡 task）：原 `/review` 复核页整体下线 —— 判定/处理的一切都在任务中心，
      // 跨 job 的「待人工看」收进任务中心工具栏的低频 Dialog（`?dialog=review-queue`）。
      // 旧深链只做重定向，不保留独立页面。
      {
        path: 'review',
        element: <Navigate to="/tasks?dialog=review-queue" replace />,
      },
      // N12 旧「处理」页（VerbRunPanel）保留路由：主导航项已撤（KI-35 P1），深链仍可达；
      // 选中集入口由 P3（task）重做成「选能力 + 方案」面板。
      {
        path: 'process',
        element: <Process />,
      },
      // 卡 task：导出不再是页面 —— 旧深链 /export 重定向到图库 + 全局导出 Dialog
      {
        path: 'export',
        element: <ExportRedirect />,
      },
      {
        path: 'settings',
        element: <Settings />,
      },
      // UI-CARRIER-1：状态型入口是页面（列表 + 分页 + 可回看），不进浮层
      {
        path: 'trash',
        element: <Trash />,
      },
      {
        path: 'tasks',
        element: <Tasks />,
      },
    ],
  },
])

function GalleryRoute() {
  return <Gallery onImport={() => window.dispatchEvent(new CustomEvent('dam:open-import'))} />
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    {/* i18n-A（task）：LocaleProvider 包最外层 —— toast/confirm 等 lib 层也要取到 locale */}
    <LocaleProvider>
      <ToastProvider>
        {/* UI-UNDO：不可逆/高成本操作的统一确认（可逆操作走 toast.undo） */}
        <ConfirmProvider>
          <TaskCenterProvider>
            <RouterProvider router={router} />
          </TaskCenterProvider>
        </ConfirmProvider>
      </ToastProvider>
    </LocaleProvider>
  </React.StrictMode>,
)

import { useCallback, useEffect, useState } from 'react'
import { tGlobal, useI18n } from './i18n'
import { Link, NavLink, Navigate, Outlet, useParams, useLocation, useNavigate } from 'react-router-dom'
import { TaskCenterButton } from './components/TaskCenter'
import { OnboardingWizard, type ImportConfig } from './components/OnboardingWizard'
import { CommandPalette } from './components/CommandPalette'
import { SmartFolderSection } from './components/SmartFolderSection'
import { ExportDialog } from './components/ExportDialog'
import { SendToProcessDialog } from './components/SendToProcessDialog'
import { PackManager } from './components/PackManager'
import { DedupScanDialog } from './components/DedupScanDialog'
import { api, type Capabilities } from './lib/api'
import { can, useCapabilities } from './lib/caps'
import { type CapabilityNavItem, useCapabilityNav } from './lib/capabilityNav'
import { OVERLAY_BASE_ATTR, Drawer, useOverlay } from './lib/modal'
import { useTasks } from './lib/tasks'
import { useToast } from './lib/toast'

/**
 * 信息架构（UI-CARRIER-1 / 卡 task，规范 UI 规范 §9.1 / §9.2.1）
 *
 * 一级导航只放「区域」：浏览（图库/搜索）、能力（已装的处理能力）、内置功能（打标）。
 * 动作型与状态型不占一级位：
 *   - 动作（导入）→ 顶栏按钮；**导出**（W 布局裁决 2026-10-19，卡 task）→ 全局 Dialog
 *     `?dialog=export&ids=…`，既不是路由页也不占一级位：它是对**已有选择集 / 当前筛选**执行一次的动作，
 *     做成页面才有「导出历史」的错觉（历史归任务中心「导出记录」）；旧深链 `/export` 重定向到该 Dialog；
 *   - 状态（任务中心 / 回收站）→ 侧栏底部「状态」区：都是**页面**（列表 + 可回看 + 分页）。
 *
 * G5-A 裁决（2026-10-19，卡 task）：失联**不做功能模块** —— 没有 Drawer、没有「核查」动作。
 * 唯一形态是图库侧栏「失联 N」行（点击 = `missing=1` 筛选）；数字由导出/读原图失败时
 * 「失败即标」写入（后端 core-storage `presence`），不是一次次全库 stat 的核查结果。
 */
/**
 * KI-35 P1（task）：主导航不再硬编码任何第二档能力的 tab —— 旧的「检查」
 * （/compliance）与「处理」（/process）两个动词 tab 撤掉，中间的能力区由**已装包
 * 注册表**驱动（useCapabilityNav）：每个已激活能力一项、显示能力名，装了出现、
 * 卸载消失；一个都没装时给安装引导。打标任务是第一档内置功能，tab 保留、不进
 * 能力区（KI-35 口径 1 / 总纲 §8 第 10 条）。
 */
const NAV = [
  { to: '/', label: '图库', icon: '▦' },
  { to: '/search', label: '搜索', icon: '⌕' },
  { to: '/tag-tasks', label: '打标任务', icon: '◈' },
]

/** 设置永远殿后（能力区插在打标任务与设置之间，不能被动态项挤走） */
const SETTINGS_ITEM = { to: '/settings', label: '设置', icon: '⚙' }

/**
 * 主导航的**静态**项（导出给单测：KI-7 要求「去重」不再占一级位 —— 它是低频处置动作，
 * 只由侧栏条件徽标 / 命令面板 / 深链进入，页面 `/dedup` 仍然保留）。
 * 能力区是动态的（已装包驱动），不在本表 —— 见 `lib/capabilityNav.ts`。
 */
export const NAV_ITEMS = [...NAV, SETTINGS_ITEM]

/**
 * 旧深链 `/export` 的落点（卡 task）：导出撤销路由页后，收藏夹/脚本里既有的
 * `/export` 链接不许 404 —— 换成 `replace` 跳到图库并把全局导出 Dialog 打开
 * （`?dialog=export`，选择集为空时 Dialog 会引导先选图或改用「当前筛选结果」）。
 */
export function ExportRedirect() {
  return <Navigate to="/?dialog=export" replace />
}

/**
 * 旧深链 `/plugins/:pluginId`（N4 详情页路由）的落点：KI-35 P1 之后能力详情页
 * 统一在 `/processing/:pluginId`（路由约定见 lib/capabilityNav.capabilityPath）。
 */
export function PluginRouteRedirect() {
  const { pluginId = '' } = useParams()
  return <Navigate to={`/processing/${encodeURIComponent(pluginId)}`} replace />
}

/**
 * 导航「能力」区（KI-35 P1）：每个已激活能力一项，显示能力名 + 一句话描述（title）；
 * 一个都没装时给安装引导（不是灰按钮 —— 没装就是没有，引导去该去的地方）。
 * 导出给单测（0/1/N 与空态引导）。
 */
export function CapabilityNavSection({ items, loaded }: { items: CapabilityNavItem[]; loaded: boolean }) {
  const { t } = useI18n()
  if (!loaded) return null
  if (items.length === 0) {
    return (
      <div
        className="px-3 py-1.5 text-[11px] leading-4 text-ink-dim max-lg:hidden"
        data-testid="nav-capabilities-empty"
      >
        {t('还没有可用的处理功能，去')}
        <Link to="/settings?drawer=packs" className="mx-0.5 text-accent hover:underline">
          {t('设置')}
        </Link>
        {t('安装')}
      </div>
    )
  }
  return (
    <>
      {items.map((it) => (
        <NavLink
          key={it.id}
          to={it.to}
          aria-label={t(it.label)}
          title={t(it.blurb || it.label)}
          data-testid={`nav-capability-${it.id}`}
          className={({ isActive }) =>
            `flex items-center gap-2 rounded-md px-3 py-2 text-sm max-lg:justify-center max-lg:px-0 ${
              isActive ? 'bg-accent-soft text-ink' : 'text-ink-dim hover:bg-panel-2 hover:text-ink'
            }`
          }
        >
          <span aria-hidden className="w-4 shrink-0 text-center text-base max-lg:w-auto">
            ✧
          </span>
          <span className="max-lg:hidden">{t(it.label)}</span>
        </NavLink>
      ))}
    </>
  )
}

const ONBOARDED_KEY = 'dam.onboarded'

/**
 * 「＋ 导入」下拉的菜单项（KI-7）：导入是高频主按钮；「仅检查重复（不入库）」是导入前的
 * 一步，收进同一个下拉里 —— 顶栏不再堆第二个图标。
 *
 * 门控：库外扫描受 `features.scanExternalPath` 约束，**false 时该项直接不出现**
 * （不是灰置：容器形态根本读不到用户本机磁盘，给个灰按钮只会招来解释成本）。
 * 导出给单测用（`importMenuItems(caps)` 纯函数）。
 */
export interface ImportMenuItem {
  id: 'import' | 'dedup-scan'
  label: string
}

export function importMenuItems(caps: Capabilities | null): ImportMenuItem[] {
  const items: ImportMenuItem[] = [{ id: 'import', label: tGlobal('导入图片…') }]
  if (can(caps, 'scanExternalPath')) {
    items.push({ id: 'dedup-scan', label: tGlobal('仅检查重复（不入库）') })
  }
  return items
}

/**
 * 侧栏「重复 N 组」条件入口（KI-7）：只在**有重复时**出现，0 组不占位。
 *
 * 计数来源是 `GET /api/dedup/groups` 的 totalGroups（全库口径）。它要把两套判据并查集
 * 合并后才算得出，所以在 App 里是**延迟拉取**（idle 后 2s）而不是首屏阻塞；
 * 裁决/导入等动作会给 `dam:library-changed`，那时再拉一次。
 */
export function DuplicateBadge({ count }: { count: number }) {
  const { t } = useI18n()
  if (!Number.isFinite(count) || count <= 0) return null
  return (
    <NavLink
      to="/dedup?range=all"
      aria-label={t('重复 {n} 组', { n: count })}
      title={t('全库有 {n} 组重复图（含精确/近似/视觉/变体）', { n: count })}
      data-testid="sidebar-dups"
      className={({ isActive }) =>
        `flex items-center gap-2 rounded-md px-3 py-1.5 text-xs max-lg:justify-center max-lg:px-0 ${
          isActive ? 'bg-accent-soft text-ink' : 'text-ink-dim hover:bg-panel-2 hover:text-ink'
        }`
      }
    >
      <span aria-hidden className="w-4 shrink-0 text-center max-lg:w-auto">
        ⧉
      </span>
      <span className="max-lg:hidden">{t('重复 {n} 组', { n: count })}</span>
      <span className="ml-auto rounded-full bg-amber-500/20 px-1.5 text-[10px] font-bold text-amber-400 max-lg:hidden">
        {count}
      </span>
    </NavLink>
  )
}

/** 全库重复组数（延迟拉取 + 库变更后刷新；失败当作 0 → 不渲染徽标，绝不假装有/没有） */
function useDuplicateGroups(): number {
  const [n, setN] = useState(0)
  useEffect(() => {
    let alive = true
    const load = () =>
      api
        .dupGroups()
        .then((r) => {
          if (alive) setN(r.totalGroups)
        })
        .catch(() => {})
    const t = window.setTimeout(load, 2000)
    const h = () => void load()
    window.addEventListener('dam:library-changed', h)
    return () => {
      alive = false
      window.clearTimeout(t)
      window.removeEventListener('dam:library-changed', h)
    }
  }, [])
  return n
}

function usePageTitle(capabilities: CapabilityNavItem[]): string {
  const { pathname } = useLocation()
  if (pathname.startsWith('/trash')) return tGlobal('回收站')
  // KI35-P3：/tasks 用户面改名「处理记录」（送去处理跑起来之后回这里看进度与结果）
  if (pathname.startsWith('/tasks')) return tGlobal('处理记录')
  // KI-7：去重不进主导航，但深链/徽标进来时标题仍然是「去重」
  if (pathname.startsWith('/dedup')) return tGlobal('去重')
  // KI-35 P1：能力详情页标题 = 能力名（未知名退「处理」）；旧 /process 页深链仍是「处理」
  if (pathname.startsWith('/processing/')) {
    return tGlobal(capabilities.find((c) => pathname.startsWith(c.to))?.label ?? '处理')
  }
  if (pathname.startsWith('/process')) return tGlobal('处理')
  const all = [...NAV, SETTINGS_ITEM, ...capabilities]
  return tGlobal(all.find((n) => (n.to === '/' ? pathname === '/' : pathname.startsWith(n.to)))?.label ?? 'DAM')
}

/** 处理插件 Drawer（`?drawer=packs`）：安装/版本/回滚/卸载 + 详情 + 该插件下的规格，>6 字段 → Drawer（§9.2.1 落点表） */
function PacksDrawer() {
  const { t } = useI18n()
  const ov = useOverlay('drawer', 'packs')
  return (
    <Drawer
      open={ov.open}
      title={t('处理插件')}
      subtitle={t("一个行业的合规检测/处理 = 一个插件：安装 .dampack、切版本、回滚、卸载；插件下建方案（要做到什么标准），判定按方案跑；历史按插件版本保留")}
      onClose={ov.closeOverlay}
      testId="packs-drawer"
      widthClass="w-[min(720px,94vw)]"
      footer={
        <button
          type="button"
          className="rounded border border-line px-3 py-1.5 text-xs text-ink-dim hover:border-accent hover:text-accent"
          onClick={ov.closeOverlay}
        >
          {t('关闭')}
        </button>
      }
    >
      <PackManager />
    </Drawer>
  )
}

export default function App() {
  const { t } = useI18n()
  // KI-35 P1：能力区导航项（已装包注册表驱动）—— 标题与导航共用同一份
  const capabilityNav = useCapabilityNav()
  const pageTitle = usePageTitle(capabilityNav.items)
  const navigate = useNavigate()
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [wizardOpen, setWizardOpen] = useState(() => localStorage.getItem(ONBOARDED_KEY) !== '1')
  const { startImport } = useTasks()
  const { toast } = useToast()
  const { tasks, runningCount } = useTasks()
  const trashLink = '/trash'
  const taskHasProblem = tasks.some((t) => t.status === 'error' || (t.status === 'done' && (t.failed ?? 0) > 0))
  // KI-7：库外扫描入口受能力位门控；重复组数延迟拉取（0 → 不渲染徽标）
  const caps = useCapabilities()
  const dupCount = useDuplicateGroups()
  const importItems = importMenuItems(caps)
  const [importMenuOpen, setImportMenuOpen] = useState(false)

  // ⌘K / Ctrl+K 全局唤起命令面板（E3）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        setPaletteOpen((v) => !v)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const openImport = useCallback(() => setWizardOpen(true), [])

  /** 顶栏「☰ 任务」→ 任务页面；带 taskId 表示从 toast 的「查看失败明细」跳过来 */
  const openTaskCenter = useCallback(
    (taskId?: number) => {
      navigate(taskId ? `/tasks?task=${taskId}` : '/tasks')
    },
    [navigate],
  )

  useEffect(() => {
    const h = () => openImport()
    window.addEventListener('dam:open-import', h)
    return () => window.removeEventListener('dam:open-import', h)
  }, [openImport])

  const finishWizard = (cfg: ImportConfig) => {
    localStorage.setItem(ONBOARDED_KEY, '1')
    setWizardOpen(false)
    void (async () => {
      try {
        // W6 修复（task）：autoTag 不再丢弃 —— 透传给 /api/import/start，
        // 批次完成后由后端入队打标并尽力启动（进度见打标任务页）。
        const { batchId } = await api.importStart(
          cfg.dir,
          cfg.mode === 'copyin' ? 'copyin' : 'reference',
          cfg.autoTag,
        )
        startImport(batchId, t('导入 {dir}', { dir: cfg.dir }), cfg.mode === 'copyin' ? t('复制入库') : t('原地索引'))
      } catch (e) {
        toast('error', t('导入启动失败'), e instanceof Error ? e.message : String(e))
        setWizardOpen(true)
      }
    })()
  }

  const closeWizard = () => {
    localStorage.setItem(ONBOARDED_KEY, '1')
    setWizardOpen(false)
    toast('info', t('可以稍后导入'), t('图库空状态下随时点「导入图片」，或用顶栏导入按钮'))
  }

  return (
    <>
      {/* data-overlay-base：浮层打开时这一层被置 inert（背景不可聚焦/不可点），浮层自己渲染在它外面 */}
      <div className="flex h-full" {...{ [OVERLAY_BASE_ATTR]: '' }}>
        {/* 侧栏导航：≥1024 完整导航；<1024 转图标条（aria-label/title 保留，§9.3） */}
        <aside className="flex w-52 shrink-0 flex-col border-r border-line bg-panel max-lg:w-14">
          <div className="px-4 py-5 max-lg:px-0 max-lg:text-center">
            <span className="text-lg font-semibold tracking-wide">DAM</span>
            <span className="ml-2 text-xs text-ink-dim max-lg:hidden">web</span>
          </div>
          <nav className="flex flex-col gap-0.5 px-2" aria-label={t('主导航')}>
            {NAV.map((item) => (
              <NavLink
                key={item.to}
                to={item.to}
                end={item.to === '/'}
                aria-label={t(item.label)}
                title={t(item.label)}
                className={({ isActive }) =>
                  `flex items-center gap-2 rounded-md px-3 py-2 text-sm max-lg:justify-center max-lg:px-0 ${
                    isActive ? 'bg-accent-soft text-ink' : 'text-ink-dim hover:bg-panel-2 hover:text-ink'
                  }`
                }
              >
                <span aria-hidden className="w-4 shrink-0 text-center text-base max-lg:w-auto">
                  {item.icon}
                </span>
                <span className="max-lg:hidden">{t(item.label)}</span>
              </NavLink>
            ))}
            {/* KI-35 P1：能力区 —— 已装能力各一项（装了出现、卸载消失），插在打标任务与设置之间 */}
            <CapabilityNavSection items={capabilityNav.items} loaded={capabilityNav.loaded} />
            <NavLink
              to={SETTINGS_ITEM.to}
              aria-label={t(SETTINGS_ITEM.label)}
              title={t(SETTINGS_ITEM.label)}
              className={({ isActive }) =>
                `flex items-center gap-2 rounded-md px-3 py-2 text-sm max-lg:justify-center max-lg:px-0 ${
                  isActive ? 'bg-accent-soft text-ink' : 'text-ink-dim hover:bg-panel-2 hover:text-ink'
                }`
              }
            >
              <span aria-hidden className="w-4 shrink-0 text-center text-base max-lg:w-auto">
                {SETTINGS_ITEM.icon}
              </span>
              <span className="max-lg:hidden">{t(SETTINGS_ITEM.label)}</span>
            </NavLink>
          </nav>
          {/* E3：智能文件夹（保存搜索）持久化入口 —— 图标条放不下，<1024 收起（命令面板可达） */}
          <div className="max-lg:hidden">
            <SmartFolderSection />
          </div>

          {/* 状态区：不占一级导航位（动作型/状态型入口集中在这里，§9.1）
              <1024 收成图标条但**仍然可达**（只隐文字，保留 aria-label/title）——
              回收站/任务不属于「可以靠命令面板兜底」的那类入口。
              G5-A：失联不在这里 —— 它只有图库侧栏「失联 N」一个入口（筛选维度，不是功能页）。 */}
          <div className="mt-auto border-t border-line px-2 py-2" data-testid="sidebar-status">
            <p className="px-3 pb-1 text-[10px] uppercase tracking-wide text-ink-dim/70 max-lg:hidden">{t('状态')}</p>
            {/* KI-7：去重只有「有重复时」才出现在这里（0 组不占位、不打扰） */}
            <DuplicateBadge count={dupCount} />
            <NavLink
              to={trashLink}
              aria-label={t('回收站')}
              title={t('回收站')}
              className={({ isActive }) =>
                `flex items-center gap-2 rounded-md px-3 py-1.5 text-xs max-lg:justify-center max-lg:px-0 ${
                  isActive ? 'bg-accent-soft text-ink' : 'text-ink-dim hover:bg-panel-2 hover:text-ink'
                }`
              }
            >
              <span aria-hidden className="w-4 shrink-0 text-center max-lg:w-auto">🗑</span>
              <span className="max-lg:hidden">{t('回收站')}</span>
            </NavLink>
            <NavLink
              to="/tasks"
              aria-label={t('处理记录')}
              title={t('处理记录')}
              className={({ isActive }) =>
                `flex items-center gap-2 rounded-md px-3 py-1.5 text-xs max-lg:justify-center max-lg:px-0 ${
                  isActive ? 'bg-accent-soft text-ink' : 'text-ink-dim hover:bg-panel-2 hover:text-ink'
                }`
              }
            >
              <span aria-hidden className="w-4 shrink-0 text-center max-lg:w-auto">☰</span>
              <span className="max-lg:hidden">{t('处理记录')}</span>
              {runningCount > 0 ? (
                <span className="ml-auto rounded-full bg-accent px-1.5 text-[10px] font-bold text-white max-lg:hidden">
                  {runningCount}
                </span>
              ) : (
                taskHasProblem && (
                  <span className="ml-auto h-2 w-2 rounded-full bg-amber-500 max-lg:hidden" aria-hidden />
                )
              )}
            </NavLink>
          </div>

          <div className="px-4 py-3 text-xs text-ink-dim max-lg:hidden">
            <button
              type="button"
              onClick={() => setPaletteOpen(true)}
              className="mb-2 flex w-full items-center justify-between rounded-md border border-line px-3 py-1.5 text-ink-dim hover:text-ink"
              title={t('命令面板')}
            >
              <span>{t('命令面板')}</span>
              <kbd className="rounded border border-line px-1">⌘K</kbd>
            </button>
            {t('v1 · 单用户')}
          </div>
        </aside>

        {/* 主区：顶栏 + 页面 */}
        <div className="flex min-w-0 flex-1 flex-col">
          <header className="flex h-12 shrink-0 items-center gap-3 border-b border-line bg-panel px-4">
            <h1 className="text-sm font-semibold">{pageTitle}</h1>
            <div className="ml-auto flex items-center gap-2">
              {/* KI-7：「＋ 导入」主按钮 + 下拉第二项「仅检查重复（不入库）」。
                  桌面形态才有下拉（能力位 false 时只剩单一导入按钮，不灰置、不占位）。 */}
              {importItems.length > 1 ? (
                <div className="relative">
                  <button
                    type="button"
                    onClick={() => setImportMenuOpen((v) => !v)}
                    aria-haspopup="menu"
                    aria-expanded={importMenuOpen}
                    data-testid="import-menu-button"
                    className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-white hover:brightness-110"
                  >
                    {t('＋ 导入')} <span aria-hidden className="ml-1 text-xs opacity-80">▾</span>
                  </button>
                  {importMenuOpen && (
                    <div
                      role="menu"
                      data-testid="import-menu"
                      className="absolute right-0 top-full z-40 mt-1 w-56 overflow-hidden rounded-md border border-line bg-panel shadow-lg"
                    >
                      {importItems.map((it) => (
                        <button
                          key={it.id}
                          type="button"
                          role="menuitem"
                          data-testid={`import-item-${it.id}`}
                          className="block w-full px-3 py-2 text-left text-xs text-ink hover:bg-panel-2"
                          onClick={() => {
                            setImportMenuOpen(false)
                            if (it.id === 'import') openImport()
                            else navigate('/?dialog=dedup-scan')
                          }}
                        >
                          {it.label}
                          {it.id === 'dedup-scan' && (
                            <span className="ml-1 block text-[10px] text-ink-dim">
                              {t('只读，不会导入库')}
                            </span>
                          )}
                        </button>
                      ))}
                    </div>
                  )}
                </div>
              ) : (
                <button
                  type="button"
                  onClick={openImport}
                  className="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-white hover:brightness-110"
                >
                  {t('＋ 导入')}
                </button>
              )}
              <TaskCenterButton onClick={openTaskCenter} />
            </div>
          </header>
          <main className="flex min-h-0 flex-1 flex-col overflow-hidden">
            <Outlet />
          </main>
        </div>
      </div>

      {/* ---------------- 全局浮层（都在 data-overlay-base 之外，否则会被自己的 inert 锁死） ---------------- */}
      {wizardOpen && <OnboardingWizard onClose={closeWizard} onFinish={finishWizard} />}
      {/* 导出配置：Dialog，`?dialog=export&ids=1,2,3` 深链直达 */}
      <ExportDialog />
      {/* KI35-P3：能力面板（送去处理）—— 选择集选能力+方案+范围，确认后建执行；`?dialog=process&ids=…` 深链直达 */}
      <SendToProcessDialog />
      {/* 处理插件（含规格）：Drawer，`?drawer=packs` */}
      <PacksDrawer />
      {/* KI-7：库外文件夹只读查重（Dialog，`?dialog=dedup-scan`；能力位 false 时不渲染内容） */}
      <DedupScanDialog />
      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        onOpenTaskCenter={() => openTaskCenter()}
      />
    </>
  )
}

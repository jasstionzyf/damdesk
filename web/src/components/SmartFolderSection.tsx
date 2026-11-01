import { useCallback, useEffect, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
import { api, type SmartFolderDto } from '../lib/api'
import { smartFolderHref } from '../lib/galleryUrlState'
import { forgetFolderCount, notifyFoldersChanged, parseQuery, querySummary, SF_EVENT } from '../lib/smartFolders'
import { useToast } from '../lib/toast'
import { SmartFolderDialog } from './SmartFolderDialog'
import { Button } from './Button'
import { useI18n } from '../i18n'

/**
 * E3 全局侧栏「智能文件夹」区：持久化入口（每项 = 一组命名筛选条件）。
 *
 * KI-16 #7（卡 t_f6d214fb）：点条目 = **图库带上保存的条件**（`/?<条件>&sf=<id>`），
 * 不再是 `/smart/:id` 那张独立浏览面；保存 / 重命名 / 删除入口都只在这里
 * （悬停 ✎🗑 + 右键菜单），删除后侧栏与深链都给出可读结果。
 */
export function SmartFolderSection() {
  const { t } = useI18n()
  const navigate = useNavigate()
  const location = useLocation()
  const { toast } = useToast()
  const [folders, setFolders] = useState<SmartFolderDto[]>([])
  const [dialog, setDialog] = useState<{ mode: 'create' } | { mode: 'edit'; folder: SmartFolderDto } | null>(
    null,
  )
  const [loaded, setLoaded] = useState(false)
  /** 右键菜单（只作用于该行；点别处 / Esc / 滚动即关） */
  const [menu, setMenu] = useState<{ x: number; y: number; folder: SmartFolderDto } | null>(null)

  const refresh = useCallback(() => {
    api
      .smartFolders()
      .then((v) => {
        setFolders(v)
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
  }, [])

  useEffect(() => {
    refresh()
    const h = () => refresh()
    window.addEventListener(SF_EVENT, h)
    window.addEventListener('focus', h)
    return () => {
      window.removeEventListener(SF_EVENT, h)
      window.removeEventListener('focus', h)
    }
  }, [refresh])

  useEffect(() => {
    if (!menu) return
    const close = () => setMenu(null)
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close()
    }
    window.addEventListener('click', close)
    window.addEventListener('contextmenu', close)
    window.addEventListener('keydown', onKey)
    window.addEventListener('scroll', close, true)
    return () => {
      window.removeEventListener('click', close)
      window.removeEventListener('contextmenu', close)
      window.removeEventListener('keydown', onKey)
      window.removeEventListener('scroll', close, true)
    }
  }, [menu])

  const remove = (f: SmartFolderDto) => {
    setMenu(null)
    api
      .smartFolderDelete(f.id)
      .then(() => {
        forgetFolderCount(f.id)
        toast('success', t('已删除智能文件夹'), f.name)
        notifyFoldersChanged()
        refresh()
      })
      .catch((e) => toast('error', t('删除失败'), e instanceof Error ? e.message : String(e)))
  }

  /** 当前是否正停在「这个文件夹的条件」上：路径是图库 + `?sf=<id>` */
  const isActive = (f: SmartFolderDto) =>
    location.pathname === '/' && new URLSearchParams(location.search).get('sf') === String(f.id)

  return (
    <div className="mt-4 px-2">
      <div className="flex items-center justify-between px-3 pb-1">
        <span className="text-xs font-semibold tracking-wider text-ink-dim">{t('智能文件夹')}</span>
        <Button
          type="button"
          title={t('新建智能文件夹')}
          aria-label={t('新建智能文件夹')}
          onClick={() => setDialog({ mode: 'create' })}
          variant="secondary"
          size="none"
          className="px-1.5 text-sm leading-5"
        >
          ＋
        </Button>
      </div>
      {!loaded && <p className="px-3 py-1 text-xs text-ink-dim">{t('加载中…')}</p>}
      {loaded && folders.length === 0 && (
        <p className="px-3 py-1 text-xs leading-5 text-ink-dim">
          {t('暂无智能文件夹')}
          <br />
          {t('点 ＋ 保存一组筛选条件')}
        </p>
      )}
      <nav className="flex flex-col gap-0.5">
        {folders.map((f) => {
          const active = isActive(f)
          const summary = querySummary(parseQuery(f.query))
          return (
            <div
              key={f.id}
              data-testid={`sf-row-${f.id}`}
              onContextMenu={(e) => {
                e.preventDefault()
                setMenu({ x: e.clientX, y: e.clientY, folder: f })
              }}
              className={`group flex items-center rounded-md ${active ? 'bg-accent-soft' : 'hover:bg-panel-2'}`}
            >
              <button
                type="button"
                title={`${summary || f.name}${summary ? t('（点击在图库中应用）') : ''}`}
                onClick={() => navigate(smartFolderHref(f.id, parseQuery(f.query)))}
                className="flex min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left text-sm"
              >
                <span className="w-4 shrink-0 text-center text-xs">{t(' 智能')}</span>
                <span className={`min-w-0 flex-1 truncate ${active ? 'text-ink' : 'text-ink-dim group-hover:text-ink'}`}>
                  {f.name}
                </span>
              </button>
              <span className="mr-1 hidden shrink-0 gap-0.5 group-hover:flex">
                <Button
                  type="button"
                  title={t('重命名 / 改条件')}
                  aria-label={t('编辑 {name}', { name: f.name })}
                  onClick={() => setDialog({ mode: 'edit', folder: f })}
                  variant="secondary"
                  size="none"
                  className="px-1 text-xs leading-5"
                >
                  ✎
                </Button>
                <Button
                  type="button"
                  title={t('删除')}
                  aria-label={t('删除 {name}', { name: f.name })}
                  onClick={() => remove(f)}
                  variant="secondary"
                  size="none"
                  tone="danger"
                  className="px-1 text-xs leading-5"
                >
                  🗑
                </Button>
              </span>
            </div>
          )
        })}
      </nav>
      {menu && (
        <div
          role="menu"
          data-testid="sf-context-menu"
          style={{ left: menu.x, top: menu.y }}
          className="fixed z-50 flex w-44 flex-col gap-0.5 rounded-md border border-line bg-panel p-1 shadow-2xl"
        >
          <Button
            type="button"
            role="menuitem"
            data-testid="sf-menu-rename"
            variant="secondary"
            size="none"
            className="w-full justify-start px-2 py-1.5 text-left text-xs"
            onClick={() => {
              setDialog({ mode: 'edit', folder: menu.folder })
              setMenu(null)
            }}
          >
            {t('重命名 / 改条件')}
          </Button>
          <Button
            type="button"
            role="menuitem"
            data-testid="sf-menu-delete"
            variant="secondary"
            size="none"
            tone="danger"
            className="w-full justify-start px-2 py-1.5 text-left text-xs"
            onClick={() => remove(menu.folder)}
          >
            {t('删除')}
          </Button>
        </div>
      )}
      {dialog && (
        <SmartFolderDialog
          folderId={dialog.mode === 'edit' ? dialog.folder.id : undefined}
          initialName={dialog.mode === 'edit' ? dialog.folder.name : undefined}
          initialQuery={dialog.mode === 'edit' ? parseQuery(dialog.folder.query) : undefined}
          onClose={() => setDialog(null)}
          onSaved={() => {
            setDialog(null)
            toast('success', t('已保存'), t('智能文件夹已更新'))
            notifyFoldersChanged()
            refresh()
          }}
        />
      )}
    </div>
  )
}

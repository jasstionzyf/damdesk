/**
 * 回收站页面 `/trash`（UI-CARRIER-1 / 卡 task；彻底删除 = KI-11 / 卡 task）
 *
 * 载体裁决：**页面**（不是 Dialog、也不是去重页里那段「展开/收起」的内嵌列表）。
 * 判据（UI 规范 §9.2.1）：回收站是「列表 + 分页 + 逐条恢复」，命中
 *   #1 字段 >6（缩略图/文件名/大小/来源/移除时间）、#4 需要可回看与分页 →
 *   任何一条不满足就不能用 Dialog；折叠段落则违背「低频但体量大的进页面」。
 *
 * 状态：本页读的是**服务端真实回收站**（`GET /api/dedup/trash`，deleted=1，最近在前）。
 * 恢复走 `POST /api/dedup/restore`（真接口，恢复后图库可见）。
 *
 * 彻底删除（KI-11）：`POST /api/dedup/purge`。语义三条：
 *   1. **点击不直接删**：先弹 `useConfirm` 确认框（动词 + 真实张数 + 「不可撤销」+ 文件影响明细），
 *      确认后才发请求（红线见 UI 规范 §9.2.2）；
 *   2. 能力位 `features.physicalDelete`（服务端实测：桌面形态 && 库根可写）为 false 时按钮灰置 +
 *      写明原因（网页容器里原图在服务器磁盘上，不伪造成功）；
 *   3. 只删回收站内的图；不在回收站的 id 由后端如实回 `skipped`，toast 里说清跳过了几张。
 */
import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { api, type AssetView } from '../lib/api'
import { can, reasonFor, useCapabilities } from '../lib/caps'
import { useConfirm, verbCount } from '../lib/confirm'
import { PathLink } from '../components/PathLink'
import { useToast } from '../lib/toast'
import { Button } from '../components/Button'
import { useI18n } from '../i18n'

const PAGE_SIZE = 20

function formatSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let v = bytes
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v >= 10 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`
}

function formatWhen(unix: number): string {
  if (!unix) return '—'
  const d = new Date(unix * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

export function Trash() {
  const { toast } = useToast()
  const confirm = useConfirm()
  const { t } = useI18n()
  const caps = useCapabilities()
  const [params, setParams] = useSearchParams()
  const [items, setItems] = useState<AssetView[]>([])
  const [sel, setSel] = useState<number[]>([])
  const [busy, setBusy] = useState(false)
  const [loaded, setLoaded] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  // 能力位（服务端实测，不由前端猜形态）：false 时按钮灰置 + 写明原因
  const purgeAllowed = can(caps, 'physicalDelete')
  const purgeReason = reasonFor(caps, 'physicalDelete')

  // 分页进 URL（`?page=2`）：可深链、刷新不丢、back 逐页回退
  const page = Math.max(1, Number(params.get('page')) || 1)

  const refresh = useCallback(async () => {
    setErr(null)
    try {
      // 后端上限 200 条/次；回收站是软删集合（去重裁决产物为主），取一屏一份即可
      const r = await api.dedupTrash(500)
      setItems(r.items)
      setSel([])
      setLoaded(true)
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
      setLoaded(true)
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const totalPages = Math.max(1, Math.ceil(items.length / PAGE_SIZE))
  const pageClamped = Math.min(page, totalPages)
  const pageItems = useMemo(
    () => items.slice((pageClamped - 1) * PAGE_SIZE, pageClamped * PAGE_SIZE),
    [items, pageClamped],
  )

  const setPage = (n: number) => {
    const next = new URLSearchParams(params)
    if (n <= 1) next.delete('page')
    else next.set('page', String(n))
    setParams(next, { replace: true })
  }

  const toggle = (assetId: number) =>
    setSel((prev) => (prev.includes(assetId) ? prev.filter((x) => x !== assetId) : [...prev, assetId]))

  const pageAllSelected = pageItems.length > 0 && pageItems.every((a) => sel.includes(a.assetId))
  const togglePageAll = () =>
    setSel((prev) =>
      pageAllSelected
        ? prev.filter((id) => !pageItems.some((a) => a.assetId === id))
        : [...new Set([...prev, ...pageItems.map((a) => a.assetId)])],
    )

  const restore = (ids: number[]) => {
    if (ids.length === 0) return
    setBusy(true)
    void (async () => {
      try {
        const r = await api.dedupRestore(ids)
        toast('success', t('已恢复 {n} 张', { n: r.restored }), t('图库中重新可见（去重页不再把它们算作待裁决）'))
        window.dispatchEvent(new CustomEvent('dam:library-changed'))
        await refresh()
      } catch (e) {
        toast('error', t('恢复失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    })()
  }

  /**
   * 彻底删除：**先确认，后执行**（不可逆操作红线）。
   * 确认框里的张数是真实选择集算的，影响明细按 storeMode 分开说（copyin 连原图删、
   * reference 只删记录）——用户点「彻底删除 3 张」前就知道自己会失去什么。
   */
  const purge = (ids: number[]) => {
    if (ids.length === 0) return
    const rows = items.filter((a) => ids.includes(a.assetId))
    const copyin = rows.filter((a) => a.storeMode === 'copyin').length
    const reference = rows.length - copyin
    void (async () => {
      const ok = await confirm({
        title: t('彻底删除 {n} 张？', { n: ids.length }),
        body: t('这些图会从库和磁盘上永久清除，回收站里也找不回来。'),
        impacts: [
          t('库记录与缩略图：{n} 张', { n: ids.length }),
          copyin > 0 ? t('库内副本原图：{n} 张（磁盘文件一并删除）', { n: copyin }) : '',
          reference > 0 ? t('原地文件：{n} 张（只删库记录，你磁盘上的原文件保留）', { n: reference }) : '',
        ].filter(Boolean),
        recoverable: t('不可撤销（回收站的「恢复」只对没被彻底删除的图有效）'),
        confirmLabel: verbCount(t('彻底删除'), ids.length),
        danger: true,
      })
      if (!ok) return
      setBusy(true)
      try {
        const r = await api.dedupPurge(ids)
        const parts: string[] = []
        if (r.filesDeleted > 0) parts.push(t('释放 {size}（{n} 个文件）', { size: formatSize(r.freedBytes), n: r.filesDeleted }))
        if (r.skipped > 0) parts.push(t('跳过 {n} 张（不在回收站）', { n: r.skipped }))
        if (r.purged > 0) {
          toast('success', t('已彻底删除 {n} 张', { n: r.purged }), parts.join(' · ') || t('库记录已清除'))
        } else {
          toast('warning', t('没有可彻底删除的图'), parts.join(' · ') || t('这批图已不在回收站'))
        }
        window.dispatchEvent(new CustomEvent('dam:library-changed'))
        await refresh()
      } catch (e) {
        toast('error', t('彻底删除失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
      }
    })()
  }

  return (
    <div className="mx-auto flex w-full max-w-5xl flex-col gap-4 overflow-y-auto p-6" data-testid="page-trash">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold">{t('回收站（{n}）', { n: items.length })}</h1>
          <p className="mt-1 text-xs text-ink-dim">
            {t('去重裁决移除的图会先落在这里（软删，不动物理文件）。恢复后重新出现在图库；回收站是低频但体量大的列表，所以是页面而不是弹窗。')}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Link
            to="/dedup"
            className="rounded-md border border-line px-3 py-1.5 text-xs text-ink-dim hover:border-accent hover:text-accent"
          >
            {t('← 回去重')}
          </Link>
          <Button
            type="button"
            disabled={busy}
            onClick={() => void refresh()}
            data-testid="trash-refresh"
            variant="secondary"
          >
            {t('刷新')}
          </Button>
        </div>
      </header>

      {err && (
        <p className="rounded border border-rose-600/60 bg-rose-600/10 px-3 py-2 text-xs text-rose-300" data-testid="trash-error">
          {t('读取回收站失败：{err}', { err })}
        </p>
      )}

      {/* 动作条：选中 ≠ 执行（选中只决定范围，必须再点一次动作） */}
      <div className="flex flex-wrap items-center gap-2 rounded-lg border border-line bg-panel px-3 py-2">
        <label className="flex select-none items-center gap-2 text-xs text-ink-dim">
          <input
            type="checkbox"
            checked={pageAllSelected}
            onChange={togglePageAll}
            disabled={pageItems.length === 0}
            data-testid="trash-select-page"
          />
          {t('全选本页（{n} 张）', { n: pageItems.length })}
        </label>
        <span className="text-xs text-ink-dim" data-testid="trash-selected-count">
          {t('已选 {n} 张', { n: sel.length })}
        </span>
        <div className="ml-auto flex items-center gap-2">
          <Button
            type="button"
            disabled={busy || sel.length === 0}
            onClick={() => restore(sel)}
            data-testid="trash-restore-selected"
            variant="primary"
          >
            {t('恢复 {n} 张', { n: sel.length })}
          </Button>
          <Button
            type="button"
            disabled={!purgeAllowed || busy || sel.length === 0}
            title={
              purgeAllowed
                ? t('彻底删除：从库和磁盘永久清除，点击后先确认（不可撤销）')
                : purgeReason || t('当前形态不支持彻底删除磁盘文件')
            }
            onClick={() => purge(sel)}
            data-testid="trash-purge-selected"
            variant="secondary"
            tone="danger"
          >
            {t('彻底删除 {n} 张', { n: sel.length })}
          </Button>
        </div>
      </div>

      <p className="text-[11px] leading-5 text-ink-dim/80" data-testid="trash-purge-note">
        {purgeAllowed
          ? t('彻底删除会先弹确认框（动词 + 真实张数 + 不可撤销），确认后才清除：库记录 + 缩略图（256/1024）；入库副本（copyin）的原图一并删除，原地引用（reference）的原文件只删记录、磁盘文件保留。')
          : t('彻底删除当前不可用：{reason}（恢复仍可用）', { reason: purgeReason || t('当前形态不支持物理删除磁盘文件') })}
      </p>

      {loaded && items.length === 0 && !err && (
        <div className="flex flex-col items-center gap-2 py-16 text-center" data-testid="trash-empty">
          <span className="text-2xl text-ink-dim">🗑</span>
          <p className="text-sm text-ink-dim">{t('回收站是空的')}</p>
          <p className="max-w-[320px] text-xs leading-5 text-ink-dim/80">
            {t('去重页裁决掉的图会先落到这里，可以随时恢复；这里为空说明没删过东西。')}
          </p>
          <Link to="/dedup" className="mt-1 text-xs text-accent hover:underline">
            {t('去去重页看看 →')}
          </Link>
        </div>
      )}

      {pageItems.length > 0 && (
        <ul className="space-y-1" data-testid="trash-list">
          {pageItems.map((a) => (
            <li
              key={a.assetId}
              className="flex items-center gap-3 rounded-md border border-line bg-panel px-2.5 py-2"
              data-testid={`trash-row-${a.assetId}`}
            >
              <input
                type="checkbox"
                checked={sel.includes(a.assetId)}
                onChange={() => toggle(a.assetId)}
                aria-label={t('选择 {v}', { v: a.fileName })}
              />
              <img
                src={api.thumbUrl(a.assetId, 256)}
                alt={a.fileName}
                loading="lazy"
                className="h-9 w-9 shrink-0 rounded object-cover"
              />
              <div className="min-w-0 flex-1">
                <p className="truncate text-xs" title={a.absPath}>
                  {a.fileName}
                </p>
                <p className="truncate text-[11px] text-ink-dim">
                  {formatSize(a.fileSz)} · {a.fileExt || a.mediaType} · {t('入库 {when}', { when: formatWhen(a.importedAt) })}
                </p>
                {/* KI-10：回收站项的原路径 = reveal（打开目录并选中该文件） */}
                <PathLink path={a.absPath} mode="reveal" />
              </div>
              <Button
                type="button"
                disabled={busy}
                onClick={() => restore([a.assetId])}
                data-testid={`trash-restore-${a.assetId}`}
                variant="secondary"
                className="shrink-0"
              >
                {t('恢复')}
              </Button>
            </li>
          ))}
        </ul>
      )}

      {/* 分页：页码进 URL，可收藏/刷新/back */}
      {items.length > PAGE_SIZE && (
        <nav className="flex items-center justify-center gap-2 text-xs" aria-label={t('回收站分页')}>
          <Button
            type="button"
            disabled={pageClamped <= 1}
            onClick={() => setPage(pageClamped - 1)}
            data-testid="trash-prev"
            variant="secondary"
          >
            {t('← 上一页')}
          </Button>
          <span className="text-ink-dim" data-testid="trash-page">
            {t('第 {a} / {b} 页（共 {c} 张）', { a: pageClamped, b: totalPages, c: items.length })}
          </span>
          <Button
            type="button"
            disabled={pageClamped >= totalPages}
            onClick={() => setPage(pageClamped + 1)}
            data-testid="trash-next"
            variant="secondary"
          >
            {t('下一页 →')}
          </Button>
        </nav>
      )}
    </div>
  )
}

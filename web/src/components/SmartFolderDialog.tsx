import { useEffect, useMemo, useState } from 'react'
import { api, type DirEntry, type SmartFolderDto, type TagCount } from '../lib/api'
import { smartFolderAssetQuery } from '../lib/galleryUrlState'
import { ModalCloseButton, useModalDismiss } from '../lib/modal'
import { COLOR_LABELS, forgetFolderCount, recordFolderCount, type FolderQuery } from '../lib/smartFolders'
import { Button } from './Button'
import { useI18n } from '../i18n'

/**
 * E3 智能文件夹新建/编辑对话框：名称 + 组合条件（标签 × 颜色 × 目录 × 未打标）。
 * 后端做白名单归一化与唯一名校验；至少一个条件（与后端 400 语义一致，前端先拦）。
 * KI-11：评分下限条件已移除（质量分级用定制标签表达）。
 */
export function SmartFolderDialog({
  folderId,
  initialName,
  initialQuery,
  onClose,
  onSaved,
}: {
  /** 有 id = 编辑（PUT），无 = 新建（POST） */
  folderId?: number
  initialName?: string
  initialQuery?: FolderQuery
  onClose: () => void
  /** 保存成功，携带服务端返回的最新 folder */
  onSaved: (folder: SmartFolderDto) => void
}) {
  const { t } = useI18n()
  const [name, setName] = useState(initialName ?? '')
  const [tag, setTag] = useState(initialQuery?.tag ?? '')
  const [color, setColor] = useState(initialQuery?.colorLabel ?? '')
  const [dir, setDir] = useState(initialQuery?.dirPrefix ?? '')
  const [untagged, setUntagged] = useState(initialQuery?.untagged ?? false)
  const [tags, setTags] = useState<TagCount[]>([])
  const [dirs, setDirs] = useState<DirEntry[]>([])
  const [err, setErr] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  // 统一关闭语义（UI-DIALOG）：Esc / ✕ / 点遮罩都能退出（未保存即丢弃，与「取消」同义）
  const { overlayProps, contentRef } = useModalDismiss({ open: true, onClose, overlayTestId: 'smart-folder-overlay' })

  useEffect(() => {
    api.tags(200).then(setTags).catch(() => setTags([]))
    api.dirs().then((r) => setDirs(r.dirs)).catch(() => setDirs([]))
  }, [])

  const query = useMemo((): FolderQuery => {
    const q: FolderQuery = {}
    if (tag) q.tag = tag
    if (color) q.colorLabel = color
    if (dir) q.dirPrefix = dir
    if (untagged) q.untagged = true
    return q
  }, [tag, color, dir, untagged])

  const hasDim = Object.keys(query).length > 0

  const save = () => {
    if (!name.trim()) {
      setErr(t('请输入名称'))
      return
    }
    if (!hasDim) {
      setErr(t('至少选择一个条件（标签 / 颜色 / 目录 / 未打标）'))
      return
    }
    setSaving(true)
    setErr(null)
    const fail = (e: unknown) => {
      setSaving(false)
      setErr(e instanceof Error ? e.message : String(e))
    }
    /**
     * KI-16 #7：保存成功后立刻记下「该条件当下命中多少张」——
     * 查询口径与图库应用条件时完全一致（`smartFolderAssetQuery`），供图库侧计数对账。
     * 对账失败（后端瞬断）不阻塞保存：清掉旧计数，图库就不显示「条件已变」假警报。
     */
    const rememberCount = (folder: SmartFolderDto) => {
      api
        .assets({ ...smartFolderAssetQuery(query), limit: 1 })
        .then((r) => recordFolderCount(folder.id, r.total))
        .catch(() => forgetFolderCount(folder.id))
        .finally(() => onSaved(folder))
    }
    // PUT/POST 响应即最新 folder，直接上抛（避免「保存后再 GET」的竞态）
    if (folderId !== undefined) {
      api.smartFolderUpdate(folderId, { name: name.trim(), query }).then(rememberCount).catch(fail)
    } else {
      api.smartFolderCreate(name.trim(), query).then(rememberCount).catch(fail)
    }
  }

  return (
    <div
      {...overlayProps}
      className="fixed inset-0 z-[70] flex items-center justify-center bg-black/60 p-4"
      role="dialog"
      aria-label={folderId !== undefined ? t('编辑智能文件夹') : t('新建智能文件夹')}
    >
      <div
        ref={contentRef}
        className="w-[440px] rounded-lg border border-line bg-panel p-5 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3">
          <h2 className="text-base font-semibold">
            {folderId !== undefined ? t('编辑智能文件夹') : t('新建智能文件夹')}
          </h2>
          <ModalCloseButton onClose={onClose} label={t('关闭智能文件夹')} />
        </div>
        <p className="mt-1 text-xs leading-5 text-ink-dim">
          {t('保存一组筛选条件为侧栏入口；点侧栏 = 图库带上这组条件（条件不落快照，随图库变化现查）。')}
        </p>

        <label className="mt-4 block text-xs font-medium text-ink-dim">{t('名称')}</label>
        <input
          autoFocus
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={t('如：精选风景')}
          maxLength={80}
          className="mt-1 w-full rounded-md border border-line bg-panel-2 px-3 py-2 text-sm outline-none focus:border-accent"
        />

        <div className="mt-3 grid grid-cols-2 gap-3">
          <div>
            <label className="block text-xs font-medium text-ink-dim">{t('标签')}</label>
            <select
              value={tag}
              onChange={(e) => setTag(e.target.value)}
              className="mt-1 w-full rounded-md border border-line bg-panel-2 px-2 py-2 text-sm outline-none focus:border-accent"
            >
              <option value="">{t('不限')}</option>
              {tags.map((t) => (
                <option key={t.value} value={t.value}>
                  {t.value}（{t.count}）
                </option>
              ))}
            </select>
          </div>
          <div>
            <label className="block text-xs font-medium text-ink-dim">{t('颜色')}</label>
            <select
              value={color}
              onChange={(e) => setColor(e.target.value)}
              className="mt-1 w-full rounded-md border border-line bg-panel-2 px-2 py-2 text-sm outline-none focus:border-accent"
            >
              <option value="">{t('不限')}</option>
              {COLOR_LABELS.map((c) => (
                <option key={c.value} value={c.value}>
                  {c.label}
                </option>
              ))}
            </select>
          </div>
          <div>
            <label className="block text-xs font-medium text-ink-dim">{t('目录')}</label>
            <select
              value={dir}
              onChange={(e) => setDir(e.target.value)}
              className="mt-1 w-full rounded-md border border-line bg-panel-2 px-2 py-2 text-sm outline-none focus:border-accent"
            >
              <option value="">{t('不限')}</option>
              {dirs.map((d) => {
                const segs = d.dir.split(/[\\/]/).filter(Boolean)
                const label = `${segs[segs.length - 1] ?? d.dir}（${d.count}）`
                return (
                  <option key={d.dir} value={d.dir}>
                    {label}
                  </option>
                )
              })}
            </select>
          </div>
        </div>

        <label className="mt-3 flex cursor-pointer items-center gap-2 text-sm">
          <input type="checkbox" checked={untagged} onChange={(e) => setUntagged(e.target.checked)} />
          {t('只看未打标')}
        </label>

        {color !== '' && (
          <p data-testid="sf-color-unsupported" className="mt-2 text-xs leading-5 text-ink-dim">
            {t('颜色条件会保存，但图库筛选没有颜色维度 —— 应用时该条件被忽略（芯片上会写明）。')}
          </p>
        )}

        {err && <p className="mt-3 rounded-md border border-red-600/50 px-3 py-2 text-xs text-red-400">{err}</p>}

        <div className="mt-5 flex justify-end gap-2">
          <Button
            type="button"
            onClick={onClose}
            variant="secondary"
            size="md"
          >
            {t('取消')}
          </Button>
          <Button
            type="button"
            disabled={saving}
            onClick={save}
            variant="primary"
            size="md"
          >
            {folderId !== undefined ? t('保存修改') : t('创建')}
          </Button>
        </div>
      </div>
    </div>
  )
}

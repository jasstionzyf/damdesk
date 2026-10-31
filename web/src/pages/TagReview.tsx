import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Link } from 'react-router-dom'
import { api, type TagReviewCard, type TagReviewItem } from '../lib/api'
import { useToast } from '../lib/toast'
import { useUrlState } from '../lib/urlState'
import { EmptyState } from '../components/StateViews'
import { Button } from '../components/Button'
import { useI18n } from '../i18n'

/** 低置信度阈值：score < 0.6 的标签黄色高亮，提示点击修改或删除 */
export const LOW_CONFIDENCE = 0.6

function fieldOf(card: TagReviewCard, kind: TagReviewItem['kind']): TagReviewItem | undefined {
  return card.items.find((i) => i.kind === kind)
}

function FieldInput({
  label,
  item,
  assetId,
  onSaved,
}: {
  label: string
  item?: TagReviewItem
  assetId: number
  onSaved: (item: TagReviewItem, value: string) => void
}) {
  const { t } = useI18n()
  const [val, setVal] = useState(item?.value ?? '')
  const dirty = useRef(false)
  useEffect(() => setVal(item?.value ?? ''), [item?.id, item?.value])

  const save = () => {
    if (!item || !dirty.current || val === item.value) return
    dirty.current = false
    void api
      .tagReviewUpdate(assetId, { updates: [{ id: item.id, value: val }] })
      .then(() => onSaved(item, val))
      .catch(() => {})
  }

  return (
    <label className="block">
      <span className="mb-0.5 block text-[10px] uppercase tracking-wide text-ink-dim">
        {label}
        {item?.edited && <span className="ml-1 text-amber-400">{t('·已改')}</span>}
      </span>
      <textarea
        data-testid={item ? `review-field-${item.id}` : undefined}
        rows={label === t('描述') ? 2 : 1}
        value={val}
        placeholder={t('（AI 未给出）')}
        onChange={(e) => {
          dirty.current = true
          setVal(e.target.value)
        }}
        onBlur={save}
        className="w-full resize-none rounded border border-line bg-panel-2 px-2 py-1 text-xs outline-none focus:border-accent"
      />
    </label>
  )
}

function TagChip({
  item,
  onRename,
  onDelete,
}: {
  item: TagReviewItem
  onRename: (id: number, name: string) => void
  onDelete: (id: number) => void
}) {
  const { t } = useI18n()
  const [editing, setEditing] = useState(false)
  const [val, setVal] = useState(item.name)
  const low = item.score < LOW_CONFIDENCE
  const inputRef = useRef<HTMLInputElement | null>(null)
  useEffect(() => {
    if (editing) inputRef.current?.focus()
  }, [editing])

  const commit = () => {
    const name = val.trim()
    setEditing(false)
    if (!name || name === item.name) {
      setVal(item.name)
      return
    }
    onRename(item.id, name)
  }

  if (editing) {
    return (
      <input
        ref={inputRef}
        data-testid={`review-tag-input-${item.id}`}
        value={val}
        onChange={(e) => setVal(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === 'Enter') commit()
          if (e.key === 'Escape') {
            setVal(item.name)
            setEditing(false)
          }
        }}
        className="w-28 rounded border border-accent bg-panel-2 px-1.5 py-0.5 text-xs outline-none"
      />
    )
  }

  return (
    <span
      data-testid={`review-tag-chip-${item.id}`}
      title={
        t('置信度 {score}', { score: item.score.toFixed(2) }) +
        (low ? t('（低置信度：点击改 / × 删）') : t(' · 点击改'))
      }
      className={`group inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs ${
        low
          ? 'cursor-pointer border-amber-500 bg-amber-500/10 text-amber-300'
          : 'cursor-pointer border-line bg-panel-2 text-ink-dim hover:text-ink'
      }`}
      onClick={() => setEditing(true)}
    >
      {item.taskName && <span className="opacity-60">{item.taskName}:</span>}
      {item.name}
      {item.edited && <span className="text-amber-400">·</span>}
      <Button
        type="button"
        aria-label={t('删除标签 {name}', { name: item.name })}
        onClick={(e) => {
          e.stopPropagation()
          onDelete(item.id)
        }}
        variant="icon"
        size="none"
        className="ml-0.5 hidden group-hover:inline-flex"
      >
        ×
      </Button>
    </span>
  )
}

function ReviewCard({
  card,
  checked,
  onCheck,
  onMutate,
  onCommit,
  onSkip,
  onPreview,
}: {
  card: TagReviewCard
  checked: boolean
  onCheck: (id: number) => void
  onMutate: (assetId: number, fn: (c: TagReviewCard) => TagReviewCard) => void
  onCommit: (assetId: number) => void
  onSkip: (assetId: number) => void
  onPreview: (assetId: number) => void
}) {
  const { t } = useI18n()
  const [newTag, setNewTag] = useState('')
  const title = fieldOf(card, 'title')
  const titleEn = fieldOf(card, 'titleEn')
  const description = fieldOf(card, 'description')
  const tags = card.items.filter((i) => i.kind === 'tag')
  const lowCount = tags.filter((t) => t.score < LOW_CONFIDENCE).length

  const saveItem = (item: TagReviewItem, value: string) => {
    onMutate(card.assetId, (c) => ({
      ...c,
      items: c.items.map((i) => (i.id === item.id ? { ...i, value, edited: true } : i)),
    }))
  }
  const renameTag = (id: number, name: string) => {
    void api.tagReviewUpdate(card.assetId, { updates: [{ id, name }] })
    onMutate(card.assetId, (c) => ({
      ...c,
      items: c.items.map((i) => (i.id === id ? { ...i, name, edited: true } : i)),
    }))
  }
  const deleteTag = (id: number) => {
    void api.tagReviewUpdate(card.assetId, { deletes: [id] })
    onMutate(card.assetId, (c) => ({ ...c, items: c.items.filter((i) => i.id !== id) }))
  }
  const addTag = () => {
    const name = newTag.trim()
    if (!name) return
    setNewTag('')
    void api.tagReviewUpdate(card.assetId, { adds: [{ kind: 'tag', name }] }).then(() => {
      // 乐观插入临时行（id 取负数占位；确认时后端按提案内容落库）
      onMutate(card.assetId, (c) => ({
        ...c,
        items: [
          ...c.items,
          {
            id: -Date.now(),
            kind: 'tag' as const,
            name,
            value: '',
            model: null,
            taskId: null,
            taskName: null,
            score: 1,
            edited: true,
          },
        ],
      }))
    })
  }

  return (
    <div
      data-testid={`review-card-${card.assetId}`}
      className={`flex flex-col rounded-lg border bg-panel p-3 ${
        checked ? 'border-accent' : 'border-line'
      }`}
    >
      <div className="flex items-start gap-3">
        <div className="relative shrink-0">
          <img
            src={api.thumbUrl(card.assetId, 256)}
            alt={card.fileName}
            loading="lazy"
            className="h-24 w-24 cursor-zoom-in rounded border border-line object-cover"
            onClick={() => onPreview(card.assetId)}
          />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <input
              type="checkbox"
              data-testid={`review-check-${card.assetId}`}
              checked={checked}
              onChange={() => onCheck(card.assetId)}
              className="accent-[var(--accent)]"
            />
            <p className="min-w-0 flex-1 truncate text-xs text-ink-dim" title={card.fileName}>
              {card.fileName}
            </p>
            {lowCount > 0 && (
              <span
                className="shrink-0 rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] text-amber-300"
                title={t('低置信度标签数：点击黄色标签修改或删除')}
              >
                {t('{n} 低置信', { n: lowCount })}
              </span>
            )}
          </div>
          <div className="mt-2 space-y-1.5">
            <FieldInput label={t('中文标题')} item={title} assetId={card.assetId} onSaved={saveItem} />
            <FieldInput label={t('英文标题')} item={titleEn} assetId={card.assetId} onSaved={saveItem} />
            <FieldInput label={t('描述')} item={description} assetId={card.assetId} onSaved={saveItem} />
          </div>
        </div>
      </div>

      <div className="mt-2 flex flex-wrap items-center gap-1.5">
        {tags.map((t) => (
          <TagChip key={t.id} item={t} onRename={renameTag} onDelete={deleteTag} />
        ))}
        <input
          data-testid={`review-add-tag-${card.assetId}`}
          value={newTag}
          onChange={(e) => setNewTag(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') addTag()
          }}
          onBlur={() => newTag.trim() && addTag()}
          placeholder={t('＋ 加标签')}
          className="w-24 rounded-full border border-dashed border-line bg-transparent px-2 py-0.5 text-xs outline-none focus:border-accent"
        />
      </div>

      <div className="mt-auto flex items-center gap-2 pt-3">
        <Button
          type="button"
          data-testid={`review-commit-card-${card.assetId}`}
          onClick={() => onCommit(card.assetId)}
          variant="secondary"
        >
          {t('确认入库')}
        </Button>
        <Button
          type="button"
          data-testid={`review-skip-card-${card.assetId}`}
          onClick={() => onSkip(card.assetId)}
          variant="secondary"
          tone="danger"
        >
          {t('跳过')}
        </Button>
        <span className="ml-auto text-[10px] text-ink-dim">
          {tags.filter((x) => x.edited).length > 0 || [title, titleEn, description].some((f) => f?.edited)
            ? t('含人工修正（按人工数据落库）')
            : t('未修改（按 AI 结果落库）')}
        </span>
      </div>
    </div>
  )
}

/** URL-STATE-1：预览浮层（open=preview&asset=<id>） */
const TAGREVIEW_DEFAULTS = { open: '', asset: 0 }

/**
 * E5 打标确认页：AI 打标结果两段式的第二段。
 * 预览（缩略图 + 提案字段/标签）→ 逐张确认/修正/跳过 → 入库；
 * 全部确认 / 全部跳过 / 勾选批量。低置信标签黄色高亮。
 */
export function TagReview() {
  const { toast } = useToast()
  const { t } = useI18n()
  const [cards, setCards] = useState<TagReviewCard[]>([])
  const [total, setTotal] = useState(0)
  const [loading, setLoading] = useState(true)
  const [checked, setChecked] = useState<Set<number>>(new Set())
  // URL-STATE-1：大图预览进 URL（open=preview&asset=<id> → 深链/刷新都能复现；back 关预览）
  const ui = useUrlState(TAGREVIEW_DEFAULTS)
  const preview = ui.value.open === 'preview' && ui.value.asset > 0 ? ui.value.asset : null
  const openPreview = useCallback(
    (assetId: number) => ui.setPush({ open: 'preview', asset: assetId }),
    [ui.setPush],
  )
  const closePreview = useCallback(() => ui.set({ open: '', asset: 0 }), [ui.set])

  const reload = useCallback(() => {
    api
      .tagReview(500)
      .then((r) => {
        setCards(r.items)
        setTotal(r.total)
        setChecked(new Set())
      })
      .catch((e) => toast('error', t('加载确认列表失败'), e instanceof Error ? e.message : String(e)))
      .finally(() => setLoading(false))
  }, [toast, t])
  useEffect(reload, [reload])

  const mutate = useCallback((assetId: number, fn: (c: TagReviewCard) => TagReviewCard) => {
    setCards((prev) => prev.map((c) => (c.assetId === assetId ? fn(c) : c)))
  }, [])

  const afterAction = useCallback(
    (title: string, detail?: string) => {
      toast('success', title, detail)
      window.dispatchEvent(new CustomEvent('dam:library-changed'))
      reload()
    },
    [reload, toast],
  )

  const commit = useCallback(
    async (ids: number[], all: boolean) => {
      try {
        const r = await api.tagReviewCommit(ids, all)
        afterAction(
          t('已确认 {n} 张入库', { n: r.report.assets }),
          t('标签 {tags} 条 · 字段 {fields} 处', { tags: r.report.tags, fields: r.report.fields }),
        )
      } catch (e) {
        toast('error', t('确认失败'), e instanceof Error ? e.message : String(e))
      }
    },
    [afterAction, toast, t],
  )

  const discard = useCallback(
    async (ids: number[], all: boolean) => {
      try {
        const r = await api.tagReviewDiscard(ids, all)
        afterAction(t('已跳过（{n} 条提案未入库）', { n: r.removed }), t('这些图保持原样，不会写入任何 AI 结果'))
      } catch (e) {
        toast('error', t('跳过失败'), e instanceof Error ? e.message : String(e))
      }
    },
    [afterAction, toast, t],
  )

  const toggleCheck = useCallback((id: number) => {
    setChecked((prev) => {
      const n = new Set(prev)
      if (n.has(id)) n.delete(id)
      else n.add(id)
      return n
    })
  }, [])

  const ids = useMemo(() => [...checked], [checked])

  if (loading) {
    return <div className="flex flex-1 items-center justify-center text-sm text-ink-dim">{t('加载确认列表…')}</div>
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* 顶栏：统计 + 批量动作 */}
      <div
        className="flex h-12 shrink-0 items-center gap-3 border-b border-line bg-panel px-4"
        data-testid="review-toolbar"
      >
        <h2 className="text-sm font-semibold" data-testid="review-total">
          {t('待确认')} <span className="text-accent">{total}</span> {t('张')}
        </h2>
        <span className="text-xs text-ink-dim">{t('AI 打标结果先在这里确认：可改字、改/删/加标签；跳过的图不会写入任何内容')}</span>
        <div className="ml-auto flex items-center gap-2">
          {ids.length > 0 && (
            <>
              <Button
                type="button"
                data-testid="review-commit-selected"
                onClick={() => void commit(ids, false)}
                variant="secondary"
              >
                {t('确认选中 {n}', { n: ids.length })}
              </Button>
              <Button
                type="button"
                data-testid="review-discard-selected"
                onClick={() => void discard(ids, false)}
                variant="secondary"
                tone="danger"
              >
                {t('跳过选中 {n}', { n: ids.length })}
              </Button>
            </>
          )}
          <Button
            type="button"
            data-testid="review-discard-all"
            onClick={() => {
              if (total > 0 && window.confirm(t('跳过全部 {n} 张？这些图的 AI 结果将被丢弃，图库不受影响。', { n: total })))
                void discard([], true)
            }}
            variant="secondary"
            tone="danger"
          >
            {t('全部跳过')}
          </Button>
          <Button
            type="button"
            data-testid="review-commit-all"
            onClick={() => {
              if (total > 0 && window.confirm(t('确认全部 {n} 张入库？未修改的按 AI 结果落库。', { n: total })))
                void commit([], true)
            }}
            variant="primary"
          >
            {t('全部确认')}
          </Button>
        </div>
      </div>

      {/* 卡片网格 */}
      <div className="min-h-0 flex-1 overflow-y-auto">
        {cards.length === 0 ? (
          <div className="flex h-full items-center justify-center">
            <EmptyState
              icon="✓"
              title={t('没有待确认的打标结果')}
              desc={t('AI 打标完成的结果都会先进入本页；当前全部处理完毕。')}
              actions={
                <Link
                  to="/tag-tasks"
                  className="rounded border border-line px-3 py-1.5 text-xs hover:border-accent hover:text-accent"
                >
                  {t('返回打标任务')}
                </Link>
              }
              hint={t('流程：打标任务页启动 → 本页逐张确认/修正 → 入库')}
            />
          </div>
        ) : (
          <div className="grid gap-3 p-3 [grid-template-columns:repeat(auto-fill,minmax(360px,1fr))]">
            {cards.map((c) => (
              <ReviewCard
                key={c.assetId}
                card={c}
                checked={checked.has(c.assetId)}
                onCheck={toggleCheck}
                onMutate={mutate}
                onCommit={(id) => void commit([id], false)}
                onSkip={(id) => void discard([id], false)}
                onPreview={openPreview}
              />
            ))}
          </div>
        )}
      </div>

      {/* 大图预览 */}
      {preview !== null && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/80 p-8"
          role="dialog"
          aria-label={t('提案大图预览')}
          onClick={closePreview}
        >
          <img
            src={api.thumbUrl(preview, 1024)}
            alt={t('预览')}
            className="max-h-full max-w-full rounded-lg object-contain"
            onClick={(e) => e.stopPropagation()}
          />
          <Button
            type="button"
            aria-label={t('关闭预览')}
            onClick={closePreview}
            variant="secondary"
            size="md"
            className="absolute right-4 top-4 rounded-full"
          >
            ✕
          </Button>
        </div>
      )}
    </div>
  )
}

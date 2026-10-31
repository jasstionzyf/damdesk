/**
 * W4 打标任务页：模板打标（dam-ai /v1/tagging 默认 + 可切 provider）+ 自定义打标任务。
 * 队列语义在后端（svc-pipeline tag/custom_tag 队列），本页只做：启动/取消/续跑/失败重试
 * + 任务 CRUD + 按模型覆盖度展示；进度统一走右上角任务中心（lib/tasks 轮询）。
 */
import { useCallback, useEffect, useRef, useState } from 'react'
import { Link } from 'react-router-dom'
import {
  api,
  type CustomTagFailedItemView,
  type CustomTagTaskDto,
  type OcrStatusView,
  type TagFailedItemView,
  type TagStatusView,
} from '../lib/api'
import { useTasks } from '../lib/tasks'
import { useToast } from '../lib/toast'
import { useUrlState } from '../lib/urlState'
import { TAG_PROMPT_FORMAT_CLAUSE, promptDeclaresOutputFormat } from '../lib/tagFormat'
import { Button, buttonClass } from '../components/Button'
import { useI18n } from '../i18n'

const card = 'rounded-lg border border-line bg-panel p-4'
const input =
  'rounded border border-line bg-panel-2 px-3 py-1.5 text-sm outline-none placeholder:text-ink-dim/70 focus:border-accent'

/**
 * URL-STATE-1（卡 task）：浮层状态进 URL。
 * open=new-task 新建 / open=edit-task&task=<id> 编辑指定任务 → 深链可直接打开编辑器，刷新不丢。
 */
const TAGTASK_DEFAULTS = { open: '', task: 0 }

function Stat({ label, value, hint }: { label: string; value: number | string; hint?: string }) {
  return (
    <div className="rounded-md border border-line bg-panel-2 px-3 py-2">
      <div className="text-xs text-ink-dim">{label}</div>
      <div className="text-lg font-semibold">{value}</div>
      {hint && <div className="text-xs text-ink-dim">{hint}</div>}
    </div>
  )
}

export function TagTasks() {
  const { startTag, tasks } = useTasks()
  const { toast } = useToast()
  const { t } = useI18n()

  const [status, setStatus] = useState<TagStatusView | null>(null)
  const [taskList, setTaskList] = useState<CustomTagTaskDto[]>([])
  const [failed, setFailed] = useState<TagFailedItemView[]>([])
  const [customFailed, setCustomFailed] = useState<CustomTagFailedItemView[]>([])
  // OCR 状态只为「模型未就绪」一行提示服务（完整 OCR 区块在设置页，卡 task）
  const [ocr, setOcr] = useState<OcrStatusView | null>(null)
  // 任务编辑对话框：开着没有由 URL 决定（open 参数），editing 只装「正在编辑的草稿」
  const ui = useUrlState(TAGTASK_DEFAULTS)
  const [editing, setEditing] = useState<{ id: number; name: string; prompt: string } | null>(null)
  const [busy, setBusy] = useState(false)

  // URL → 对话框草稿：只在 open/task 变化时重建（轮询刷新 taskList 不冲掉正在输入的内容）
  const appliedOpen = useRef('')
  const openEditor = useCallback(
    (id?: number) => ui.setPush({ open: id ? 'edit-task' : 'new-task', task: id ?? 0 }),
    [ui.setPush],
  )
  const closeEditor = useCallback(() => ui.set({ open: '', task: 0 }), [ui.set])
  /**
   * URL → 对话框草稿（URL-STATE-1 + cold-load 修复 · 卡 task）
   *
   * 关键点：`appliedOpen` 只在「草稿真的建好了」时才记 key。冷加载深链
   * `/tag-tasks?open=edit-task&task=N` 首轮 effect 跑时 taskList 还是空的（API 未返回），
   * 旧实现先记 key 再 setEditing(null) → taskList 到达后同 key 被 guard 挡住，对话框永不出现。
   * 现在：
   *   - edit-task 且 taskList 里还没这条 → 不记 key（保持关闭），等 taskList 更新重试；
   *   - 同 key 且草稿已在（用户正在输入 / 已重建）→ 不覆盖输入，只挡这一种情况；
   *   - 同 key 但草稿为空（冷加载竞态/草稿被清）→ 重放 setEditing。
   */
  useEffect(() => {
    const key = `${ui.value.open}:${ui.value.task}`
    if (ui.value.open === 'new-task') {
      if (key === appliedOpen.current) return
      appliedOpen.current = key
      // 统一标签裁决：新建任务预填输出格式段（值数组 + `_` 连接），用户在其上补提问
      setEditing({ id: 0, name: '', prompt: TAG_PROMPT_FORMAT_CLAUSE })
      return
    }
    if (ui.value.open === 'edit-task') {
      const t = taskList.find((x) => x.id === ui.value.task)
      // 任务列表未到 / 任务确已不存在：不记 key，taskList 下次变化时再试（无匹配就不开，不炸）
      if (!t) {
        setEditing(null)
        return
      }
      if (key === appliedOpen.current && editing !== null) return
      appliedOpen.current = key
      setEditing({ id: t.id, name: t.name, prompt: t.prompt })
      return
    }
    appliedOpen.current = key
    setEditing(null)
  }, [ui.value.open, ui.value.task, taskList, editing])

  const templateRunning = tasks.some(
    (t) => t.kind === 'tag' && t.tagVariant !== 'custom' && t.status === 'running',
  )
  const customRunning = tasks.some(
    (t) => t.kind === 'tag' && t.tagVariant === 'custom' && t.status === 'running',
  )

  const refresh = useCallback(async () => {
    try {
      const [st, list, f, cf, os] = await Promise.all([
        api.tagStatus(),
        api.tagTasks(),
        api.tagFailed(),
        api.tagCustomFailed(),
        api.ocrStatus(),
      ])
      setStatus(st)
      setTaskList(list.items)
      setFailed(f.items)
      setCustomFailed(cf.items)
      setOcr(os)
    } catch {
      /* 后端瞬断：保留上次快照 */
    }
  }, [])

  useEffect(() => {
    void refresh()
    // 任务跑完（dam:library-changed）或轮询期变化后刷新统计
    const h = () => void refresh()
    window.addEventListener('dam:library-changed', h)
    const timer = window.setInterval(h, 5000)
    return () => {
      window.removeEventListener('dam:library-changed', h)
      window.clearInterval(timer)
    }
  }, [refresh])

  const run = (fn: () => Promise<unknown>, ok: string) =>
    void (async () => {
      setBusy(true)
      try {
        await fn()
        toast('info', ok, '')
      } catch (e) {
        toast('error', t('操作失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
        void refresh()
      }
    })()

  const startTemplate = (mode: 'pending' | 'all') =>
    void (async () => {
      setBusy(true)
      try {
        const r = await api.tagStart(mode)
        startTag(
          'template',
          mode === 'all' ? t('模板打标（全库重打）') : t('模板打标（增量）'),
          t('{n} 张 · {model}', { n: r.queued, model: status?.provider.activeModel || '' }),
        )
        toast('info', t('已开始打标'), t('入队 {n} 张，进度见任务中心', { n: r.queued }))
      } catch (e) {
        toast('error', t('启动失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
        void refresh()
      }
    })()

  const runTask = (task: CustomTagTaskDto, mode: 'all' | 'resume') =>
    void (async () => {
      setBusy(true)
      try {
        const r = await api.tagTaskRun(task.id, mode)
        startTag('custom', t('自定义打标：{name}', { name: task.name }), t('{n} 张待处理', { n: r.queued }))
        toast('info', t('任务「{name}」已开始', { name: task.name }), t('入队 {n} 张，进度见任务中心', { n: r.queued }))
      } catch (e) {
        toast('error', t('启动失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
        void refresh()
      }
    })()

  const saveTask = () =>
    void (async () => {
      if (!editing) return
      // 统一标签裁决（2026-10-19）：prompt 未声明输出格式（值数组 + `_` 连接）时默认阻止保存，
      // 弹确认说明后果，用户显式确认才放行（window.confirm 默认 = 取消 = 阻止）。
      if (!promptDeclaresOutputFormat(editing.prompt)) {
        const ok = window.confirm(
          t('提示词未声明输出格式（离散标签数组 + 多词下划线连接）。\n不声明的话模型可能输出整段文字，标签将无法自解释、分组展示会失效。\n\n仍要保存吗？（建议先点「插入格式要求」补上）'),
        )
        if (!ok) return
      }
      setBusy(true)
      try {
        if (editing.id > 0) {
          await api.tagTaskUpdate(editing.id, editing.name, editing.prompt, true)
        } else {
          await api.tagTaskCreate(editing.name, editing.prompt, true)
        }
        closeEditor()
        toast('success', t('任务已保存'), '')
      } catch (e) {
        toast('error', t('保存失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
        void refresh()
      }
    })()

  const deleteTask = (task: CustomTagTaskDto) =>
    void (async () => {
      const purge = window.confirm(
        t('删除任务「{name}」？\n确定 = 连历史标签一起清理；取消 = 保留历史标签（仅断溯源）', { name: task.name }),
      )
      setBusy(true)
      try {
        const r = await api.tagTaskDelete(task.id, purge)
        toast(
          'success',
          t('任务已删除'),
          purge ? t('清理了 {n} 条历史标签', { n: r.purgedTags }) : t('历史标签已保留（溯源已断开）'),
        )
      } catch (e) {
        toast('error', t('删除失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
        void refresh()
      }
    })()

  const p = status?.provider
  const stats = status?.stats
  /** 失败分桶：「截断（max_tokens 预算）」与其它（网络/上游/非法输出）分开重试 */
  const truncatedFailed = failed.filter((f) => f.errorClass === 'truncated')
  const otherFailed = failed.length - truncatedFailed.length

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-4">
      {/* E5 确认流入口：有待确认提案时置顶提示（AI 结果不再直接入库） */}
      {(status?.review?.assets ?? 0) > 0 && (
        <div
          className={`${card} flex items-center gap-3 border-amber-500/50 bg-amber-500/5`}
          data-testid="tag-review-banner"
        >
          <span className="text-lg">🏷️</span>
          <div className="min-w-0 flex-1">
            <div className="text-sm font-semibold text-amber-300">
              {t('{n} 张打标结果待确认', { n: status!.review.assets })}
            </div>
            <div className="mt-0.5 text-xs text-ink-dim">
              {t('AI 结果已生成但尚未入库 —— 去确认页逐张核对（可修改/删除/跳过），确认后才写入图库')}
            </div>
          </div>
          <Link
            to="/tag-review"
            data-testid="tag-review-banner-link"
            className="rounded bg-accent px-3 py-1.5 text-xs font-medium text-white hover:brightness-110"
          >
            {t('去确认')}
          </Link>
        </div>
      )}
      {/* provider 状态条（WIN-PRE2：configured 但缺 Key 也要显式引导，不许只靠失败计数） */}
      <div className={`${card} flex flex-wrap items-center gap-3`}>
        <div className="min-w-0 flex-1">
          <div className="text-sm font-semibold">{t('打标引擎')}</div>
          {p?.configured && p.hasKey ? (
            <div className="mt-1 text-xs text-ink-dim">
              {p.enabled ? '' : t('未启用 · ')}
              <code className="text-ink">{p.activeModel}</code> · {p.baseUrl}
              <span className="ml-2">
                max_tokens{' '}
                <code className="text-ink">
                  {p.maxTokensLabel || `${p.maxTokens ?? 2048}`}
                </code>
              </span>
              {p.maxTokensSource === 'settings' && (
                <span className="ml-1 text-amber-400">
                  {t('（改小了容易截断，建议 ≥ {n}）', { n: p.defaultMaxTokens ?? 2048 })}
                </span>
              )}
            </div>
          ) : p?.configured ? (
            <div className="mt-1 text-xs text-ink-dim">
              {t('打标端点 ')}<code className="text-ink">{p.baseUrl}</code>{t(' 已预填，但')}{' '}
              <span className="font-medium text-ink">{t('API Key 未填')}</span>{t(' —— 到')}{' '}
              <Link to="/settings" className="text-accent hover:underline">
                {t('设置 → AI 打标')}
              </Link>{' '}
              {t('粘贴 API Key 并「测试连接」后才能打标')}
            </div>
          ) : (
            <div className="mt-1 text-xs text-ink-dim">
              {t('打标 provider 未配置 —— 先到')}{' '}
              <Link to="/settings" className="text-accent hover:underline">
                {t('设置 → AI 打标')}
              </Link>{' '}
              {t('选择 provider、粘贴 API Key 并测试连接')}
            </div>
          )}
        </div>
        {p && !p.enabled && p.configured && p.hasKey && (
          <Link to="/settings" className={buttonClass({ variant: 'secondary' })}>
            {t('去启用')}
          </Link>
        )}
        {p && p.configured && !p.hasKey && (
          <Link to="/settings" className={buttonClass({ variant: 'secondary' })}>
            {t('去填 API Key')}
          </Link>
        )}
      </div>

      {/* OCR 一能力一处（卡 task）：完整区块在设置页「文字识别（OCR）」；
          这里只留一行可点提示，条件 = 后端常驻报模型未就绪（不是等点了才吃 409）。 */}
      {ocr && ocr.modelsReady === false && (
        <Link
          to="/settings#ocr"
          data-testid="ocr-not-ready-hint"
          className={`${card} flex items-center gap-2 border-amber-500/50 bg-amber-500/5 text-xs text-amber-200 hover:brightness-110`}
        >
          <span aria-hidden className="text-sm">
            🔤
          </span>
          <span className="min-w-0 flex-1">
            {t('文字识别（OCR）模型未就绪 —— 识别与文字检索暂不可用（打标本身不受影响）。点这里去设置页「文字识别（OCR）」处理 →')}
          </span>
        </Link>
      )}
      {/* 模板打标 */}
      <section className={card}>
        <div className="flex flex-wrap items-center gap-2">
          <div className="text-sm font-semibold">{t('模板打标')}</div>
          <span className="text-xs text-ink-dim">
            {t('三合一（中文标题 / 英文标题 / 描述 / 关键词），模板可在设置页改写')}
          </span>
          <div className="ml-auto flex flex-wrap gap-2">
            <Button
              type="button"
              disabled={busy || templateRunning || !p?.enabled || !p?.configured}
              onClick={() => startTemplate('pending')}
              variant="primary"
            >
              {t('增量打标')}
            </Button>
            <Button
              type="button"
              disabled={busy || templateRunning || !p?.enabled || !p?.configured}
              onClick={() => {
                if (
                  window.confirm(
                    t('全库重打会覆盖全部自动标签（人工标签不动），继续？'),
                  )
                ) {
                  startTemplate('all')
                }
              }}
              variant="secondary"
            >
              {t('全库重打')}
            </Button>
            <Button
              type="button"
              disabled={!templateRunning}
              onClick={() => run(() => api.tagCancel(), t('已请求取消'))}
              variant="secondary"
            >
              {t('取消')}
            </Button>
            <Button
              type="button"
              disabled={busy || failed.length === 0}
              onClick={() =>
                run(async () => {
                  const r = await api.tagRetryFailed()
                  toast(
                    'info',
                    t('失败项已重置'),
                    t('{n} 条回 pending', { n: r.reset }) + (r.started ? t('，已自动续跑') : ''),
                  )
                }, t('失败项已重置'))
              }
              variant="secondary"
            >
              {t('重试失败（{n}）', { n: failed.length })}
            </Button>
            {truncatedFailed.length > 0 && otherFailed > 0 && (
              <Button
                type="button"
                disabled={busy}
                title={t('只重试被 max_tokens 截断的失败项（先把设置页 max_tokens 调大再点）')}
                onClick={() =>
                  run(async () => {
                    const r = await api.tagRetryFailed('truncated')
                    toast(
                      'info',
                      t('截断类已重置'),
                      t('{n} 条回 pending', { n: r.reset }) + (r.started ? t('，已自动续跑') : ''),
                    )
                  }, t('截断类失败项已重置'))
                }
                variant="secondary"
              >
                {t('仅重试截断（{n}）', { n: truncatedFailed.length })}
              </Button>
            )}
          </div>
        </div>
        {stats && (
          <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-4">
            <Stat label={t('待打标图片')} value={stats.untaggedAssets} hint={t('无自动标签')} />
            <Stat label={t('队列 pending')} value={stats.pending} />
            <Stat label={t('本轮队列 done')} value={stats.done} />
            <Stat label={t('队列 failed')} value={stats.failed} />
          </div>
        )}
        {(status?.template.progress || status?.template.report) && (
          <div className="mt-2 text-xs text-ink-dim">
            {status?.template.progress && (
              <div>
                {t('进度：{i}/{total} · 成功 {done} · 失败 {failed}', {
                  i: status.template.progress.index,
                  total: status.template.progress.total,
                  done: status.template.progress.doneCount,
                  failed: status.template.progress.failedCount,
                })}
                {status.template.progress.error ? t(' · 最近错误：{err}', { err: status.template.progress.error }) : ''}
              </div>
            )}
            {status?.template.report && (
              <div className="mt-1">
                {t('上轮报告：处理 {p} · 成功 {done} · 失败 {failed} · 跳过 {skipped}', {
                  p: status.template.report.processed,
                  done: status.template.report.done,
                  failed: status.template.report.failed,
                  skipped: status.template.report.skipped,
                })}
                {(status.template.report.repaired ?? 0) > 0 && (
                  <span className="text-amber-400">
                    {' '}
                    {t('· 截断补齐 {n}（标签可能变少，建议调大 max_tokens）', { n: status.template.report.repaired ?? 0 })}
                  </span>
                )}
                {status.template.report.cancelled ? t(' · 已取消') : ''}
              </div>
            )}
          </div>
        )}
        {/* 按模型覆盖度（tagModel 隔离的可见面） */}
        {status && status.models.length > 0 && (
          <div className="mt-3">
            <div className="text-xs text-ink-dim">{t('按模型覆盖度（自动标签）')}</div>
            <div className="mt-1 flex flex-wrap gap-2">
              {status.models.map((m) => (
                <span
                  key={m.model || 'none'}
                  className="rounded border border-line bg-panel-2 px-2 py-1 text-xs"
                >
                  <code>{m.model || t('(未知模型)')}</code>{t(' · {n} 张', { n: m.assets })}
                  {p && m.model === p.activeModel && (
                    <span className="ml-1 text-accent">{t('当前')}</span>
                  )}
                </span>
              ))}
            </div>
          </div>
        )}
        {failed.length > 0 && (
          <div className="mt-3 max-h-40 overflow-y-auto rounded-md border border-line bg-panel-2 p-2">
            {failed.slice(0, 20).map((f) => (
              <div key={f.jobId} className="text-xs text-ink-dim">
                <span
                  className={
                    f.errorClass === 'truncated'
                      ? 'mr-1 rounded bg-amber-500/15 px-1 text-amber-400'
                      : 'mr-1 rounded bg-panel px-1'
                  }
                >
                  {f.errorClass === 'truncated' ? t('截断') : t('其它')}
                </span>
                <span className="break-all">
                  {t('asset {id} · 尝试 {n} · {err}', { id: f.assetId, n: f.attempts, err: f.lastError })}
                </span>
              </div>
            ))}
          </div>
        )}
      </section>


      {/* 自定义打标任务 */}
      <section className={card}>
        <div className="flex flex-wrap items-center gap-2">
          <div className="text-sm font-semibold">{t('自定义打标任务')}</div>
          <span className="text-xs text-ink-dim">
            {t('每个任务 = 一个标签维度（inline prompt 通道），结果写入标签并可用于过滤')}
          </span>
          <div className="ml-auto flex flex-wrap gap-2">
            <Button
              type="button"
              onClick={() => openEditor()}
              variant="secondary"
            >
              {t('＋ 新建任务')}
            </Button>
            <Button
              type="button"
              disabled={busy || customRunning || status?.custom.pendingJobs === 0}
              onClick={() =>
                void (async () => {
                  const target = taskList.find((t) => t.jobPending > 0) ?? taskList[0]
                  if (!target) {
                    toast('info', t('没有任务'), t('先创建一个自定义打标任务'))
                    return
                  }
                  runTask(target, 'resume')
                })()
              }
              variant="secondary"
            >
              {t('续跑')}{status?.custom.pendingJobs ? t('（{n}）', { n: status.custom.pendingJobs }) : ''}
            </Button>
            <Button
              type="button"
              disabled={!customRunning}
              onClick={() => run(() => api.tagCustomCancel(), t('已请求取消'))}
              variant="secondary"
            >
              {t('取消')}
            </Button>
            <Button
              type="button"
              disabled={busy || customFailed.length === 0}
              onClick={() =>
                run(async () => {
                  const r = await api.tagCustomRetryFailed()
                  toast('info', t('失败项已重置'), t('{n} 条回 pending', { n: r.reset }))
                }, t('失败项已重置'))
              }
              variant="secondary"
            >
              {t('重试失败（{n}）', { n: customFailed.length })}
            </Button>
          </div>
        </div>

        {taskList.length === 0 ? (
          <div className="mt-3 rounded-md border border-dashed border-line px-4 py-6 text-center text-xs text-ink-dim">
            {t('还没有自定义任务。点「＋ 新建任务」创建，例如：任务名「拍摄地点」，提示词「这张照片的拍摄地点是哪里？用简短中文回答」——打完后即可在图库按该维度过滤。')}
          </div>
        ) : (
          <div className="mt-3 overflow-x-auto">
            <table className="w-full text-left text-xs">
              <thead className="text-ink-dim">
                <tr className="border-b border-line">
                  <th className="py-2 pr-3 font-medium">{t('任务')}</th>
                  <th className="py-2 pr-3 font-medium">{t('提示词')}</th>
                  <th className="py-2 pr-3 font-medium">{t('状态')}</th>
                  <th className="py-2 pr-3 font-medium">{t('队列')}</th>
                  <th className="py-2 pr-3 font-medium">{t('最近执行')}</th>
                  <th className="py-2 pr-3 font-medium">{t('操作')}</th>
                </tr>
              </thead>
              <tbody>
                {taskList.map((task) => (
                  <tr key={task.id} className="border-b border-line/60">
                    <td className="py-2 pr-3">
                      <span className="font-medium">{task.name}</span>
                      {!task.enabled && <span className="ml-1 text-ink-dim">{t('（停用）')}</span>}
                    </td>
                    <td className="max-w-[280px] truncate py-2 pr-3 text-ink-dim">{task.prompt}</td>
                    <td className="py-2 pr-3">
                      {task.jobPending > 0
                        ? customRunning
                          ? t('进行中')
                          : t('待续跑')
                        : task.jobDone > 0
                          ? t('已完成')
                          : t('未跑过')}
                    </td>
                    <td className="py-2 pr-3 text-ink-dim">
                      {task.jobPending}/{task.jobDone}/{task.jobFailed}
                    </td>
                    <td className="py-2 pr-3 text-ink-dim">
                      {task.lastRunAt ? new Date(task.lastRunAt * 1000).toLocaleString() : '—'}
                    </td>
                    <td className="py-2 pr-3">
                      <div className="flex gap-1.5">
                        <Button
                          type="button"
                          disabled={busy || customRunning || !p?.enabled || !p?.configured}
                          onClick={() => runTask(task, 'all')}
                          variant="secondary"
                        >
                          {t('全量重跑')}
                        </Button>
                        <Button
                          type="button"
                          disabled={busy || customRunning}
                          onClick={() => openEditor(task.id)}
                          variant="secondary"
                        >
                          {t('编辑')}
                        </Button>
                        <Button
                          type="button"
                          disabled={busy || customRunning}
                          onClick={() => deleteTask(task)}
                          variant="secondary"
                          tone="danger"
                        >
                          {t('删除')}
                        </Button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {(status?.custom.progress || status?.custom.report) && (
          <div className="mt-2 text-xs text-ink-dim">
            {status?.custom.progress && (
              <div>
                {t('进度：{i}/{total} · 成功 {done} · 失败 {failed}', {
                  i: status.custom.progress.index,
                  total: status.custom.progress.total,
                  done: status.custom.progress.doneCount,
                  failed: status.custom.progress.failedCount,
                })}
              </div>
            )}
            {status?.custom.report && (
              <div className="mt-1">
                {t('上轮报告：处理 {p} · 成功 {done} · 失败 {failed} · 空值/跳过 {skipped}', {
                  p: status.custom.report.processed,
                  done: status.custom.report.done,
                  failed: status.custom.report.failed,
                  skipped: status.custom.report.skipped,
                })}
                {status.custom.report.cancelled ? t(' · 已取消') : ''}
              </div>
            )}
          </div>
        )}
        {customFailed.length > 0 && (
          <div className="mt-3 max-h-40 overflow-y-auto rounded-md border border-line bg-panel-2 p-2">
            {customFailed.slice(0, 20).map((f) => (
              <div key={f.jobId} className="truncate text-xs text-ink-dim">
                {t('[{task}] asset {id} · 尝试 {n} · {err}', { task: f.taskName, id: f.assetId, n: f.attempts, err: f.lastError })}
              </div>
            ))}
          </div>
        )}
      </section>

      {/* 任务编辑对话框 */}
      {editing && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
          data-testid="tagtask-editor"
          onClick={closeEditor}
        >
          <div
            className="w-full max-w-lg rounded-lg border border-line bg-panel p-4"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="text-sm font-semibold">
              {editing.id > 0 ? t('编辑任务') : t('新建自定义打标任务')}
            </div>
            <div className="mt-3 flex flex-col gap-3">
              <label className="flex flex-col gap-1 text-xs text-ink-dim">
                {t('任务名（= 标签名，库内唯一）')}
                <input
                  className={input}
                  value={editing.name}
                  onChange={(e) => setEditing({ ...editing, name: e.target.value })}
                  placeholder={t('拍摄地点')}
                  maxLength={64}
                />
              </label>
              <label className="flex flex-col gap-1 text-xs text-ink-dim">
                {t('提示词（模型按此逐图分析；空值图片不写标签）')}
                <textarea
                  className={`${input} min-h-24`}
                  value={editing.prompt}
                  onChange={(e) => setEditing({ ...editing, prompt: e.target.value })}
                  placeholder={t('这张照片的拍摄地点是哪里？输出为离散标签数组，如 ["青岛", "崂山"]')}
                />
              </label>
              <div className="flex items-center justify-between gap-2">
                <span className="text-[11px] leading-4 text-ink-dim">
                  {t('模型输出必须是离散标签数组，多词用 _ 连接 —— 否则标签无法自解释、不能按 key-val 分组')}
                </span>
                {!editing.prompt.includes(TAG_PROMPT_FORMAT_CLAUSE) && (
                  <Button
                    type="button"
                    data-testid="tagtask-insert-format"
                    variant="secondary"
                    onClick={() =>
                      setEditing({
                        ...editing,
                        prompt: `${editing.prompt.trim()}${editing.prompt.trim() ? '\n' : ''}${TAG_PROMPT_FORMAT_CLAUSE}`,
                      })
                    }
                  >
                    {t('插入格式要求')}
                  </Button>
                )}
              </div>
              <div className="flex justify-end gap-2">
                <Button
                  type="button"
                  onClick={closeEditor}
                  variant="secondary"
                >
                  {t('取消')}
                </Button>
                <Button
                  type="button"
                  disabled={busy || !editing.name.trim() || !editing.prompt.trim()}
                  onClick={saveTask}
                  variant="primary"
                >
                  {t('保存')}
                </Button>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}

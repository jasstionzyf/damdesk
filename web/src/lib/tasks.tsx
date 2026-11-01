import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react'
import { api, type ImportFailure, type ImportProgress } from './api'
import { useToast } from './toast'
import { tGlobal } from '../i18n'

export type TaskKind = 'import' | 'embed' | 'tag' | 'dedup' | 'export'
export type TaskStatus = 'running' | 'done' | 'error' | 'cancelled'

export interface Task {
  id: number
  kind: TaskKind
  title: string
  detail: string
  current: number
  total: number
  status: TaskStatus
  error?: string
  /** 导入：本批次失败张数（>0 ⇒ 部分失败，行内按警告色渲染） */
  failed?: number
  /** 导入：失败明细（哪张、为什么）——「重试失败项」的数据源 */
  failedItems?: ImportFailure[]
  startedAt: number
  /** 后端批次号（导入任务真实驱动；W3+ 其余任务逐类接入） */
  batchId?: number
  /** W4：打标任务分槽（模板队列 / 自定义任务队列） */
  tagVariant?: 'template' | 'custom'
}

interface TaskApi {
  tasks: Task[]
  runningCount: number
  /** 启动真实导入任务：进度轮询 /api/import/status，完成时派发 dam:library-changed */
  startImport: (batchId: number, title: string, detail: string) => number
  /** W3：启动真实嵌入任务（后端 /api/embed/start），进度轮询 /api/embed/status */
  startEmbed: (title: string, detail: string) => number
  /** W4：登记真实打标任务（进度轮询 /api/tag/status 对应槽位） */
  startTag: (variant: 'template' | 'custom', title: string, detail: string) => number
  cancel: (id: number) => void
  retry: (id: number) => void
  /** 导入任务：只重跑本批失败的那几张（不重扫目录），另起一条任务行 */
  retryFailed: (id: number) => void
  dismiss: (id: number) => void
}

const TaskCtx = createContext<TaskApi | null>(null)

let nextId = 1

export const TASK_KIND_LABEL: Record<TaskKind, string> = {
  import: '导入',
  embed: '嵌入',
  tag: '打标',
  dedup: '去重',
  export: '导出',
}

/**
 * i18n-D：任务类型标签取词入口（渲染时经 tGlobal 按当前 locale 翻译）。
 * 旧的模块级常量 `TASK_KIND_LABEL` 在模块加载时固化译文（locale 切换后不更新），已废弃；
 * 消费方（TaskCenter 等）一律改调本函数。
 */
export function taskKindLabel(kind: TaskKind): string {
  return tGlobal(TASK_KIND_LABEL[kind])
}

const POLL_MS = 1000

/**
 * 任务中心（W2 起真实驱动）：导入任务绑定后端批次，单一轮询循环同步
 * 进度/完成/取消/失败；完成或失败发 toast + `dam:library-changed` 事件
 * （图库页监听后自动刷新）。W3+ 嵌入/打标/去重/导出任务逐类换到同一机制。
 */
export function TaskCenterProvider({ children }: { children: React.ReactNode }) {
  const [tasks, setTasks] = useState<Task[]>([])
  const { toast } = useToast()
  const inflight = useRef(false)

  const hasRunning = tasks.some((t) => t.status === 'running')

  // 单一轮询循环：有任何 running 任务时每秒拉一次后端状态
  useEffect(() => {
    if (!hasRunning) return
    let stop = false

    const tick = async () => {
      if (stop || inflight.current) return
      inflight.current = true
      try {
        const st = await api.importStatus()
        if (stop) return
        setTasks((prev) =>
          prev.map((t) => {
            if (t.status !== 'running' || t.batchId === undefined) return t
            const p: ImportProgress | null = st.progress
            if (p && p.batchId === t.batchId) {
              return {
                ...t,
                current: p.done,
                total: p.total,
                failed: p.failed,
                failedItems: p.failedItems ?? [],
              }
            }
            return t
          }),
        )
        if (!st.running) {
          // 批次结束：按 outcome 归档所有绑定了批次的 running 任务
          const outcome = st.outcome
          setTasks((prev) =>
            prev.map((t) => {
              if (t.status !== 'running' || t.batchId === undefined) return t
              if (!outcome) return t
              if (outcome.batchId !== t.batchId) return t
              if (outcome.status === 'done') {
                window.dispatchEvent(new CustomEvent('dam:library-changed'))
                const failed = outcome.failed ?? 0
                const items = outcome.failedItems ?? []
                if (failed > 0) {
                  // 部分失败：不再报「✓ 完成」，改为警告色 + 直接给出「哪张、为什么」，
                  // 并给一条去任务中心看明细的入口（失败项可单独重试）。
                  const first = items[0]
                  toast(
                    'warning',
                    tGlobal('{title}：{n} 张失败', { title: t.title, n: failed }),
                    tGlobal('{n} 张成功 · 重复 {duplicates}', {
                      n: outcome.imported,
                      duplicates: outcome.duplicates,
                    }) +
                      (first
                        ? tGlobal(' · 首个失败 {name}：{err}', { name: first.fileName, err: first.error })
                        : ''),
                    {
                      label: tGlobal('查看失败明细'),
                      onClick: () =>
                        window.dispatchEvent(
                          new CustomEvent('dam:open-task-center', { detail: { taskId: t.id } }),
                        ),
                    },
                  )
                  return {
                    ...t,
                    status: 'done',
                    failed,
                    failedItems: items,
                    current: t.total || outcome.total,
                    total: outcome.total || t.total,
                  }
                }
                toast(
                  'success',
                  tGlobal('{title} 完成', { title: t.title }),
                  tGlobal('导入 {n} · 重复 {duplicates} · 失败 0', {
                    n: outcome.imported,
                    duplicates: outcome.duplicates,
                  }),
                )
                return { ...t, status: 'done', current: t.total, total: outcome.total || t.total, failed: 0, failedItems: [] }
              }
              if (outcome.status === 'cancelled') {
                toast('info', tGlobal('{title} 已取消', { title: t.title }), tGlobal('可在任务中心续跑'))
                window.dispatchEvent(new CustomEvent('dam:library-changed'))
                return { ...t, status: 'cancelled' }
              }
              const items = outcome.failedItems ?? []
              const msg = items[0]
                ? tGlobal('批次 {batchId} {status}：{name} — {err}', {
                    batchId: t.batchId,
                    status: outcome.status,
                    name: items[0].fileName,
                    err: items[0].error,
                  })
                : tGlobal('批次 {batchId} {status}', { batchId: t.batchId, status: outcome.status })
              toast('error', tGlobal('{title} 失败', { title: t.title }), tGlobal('{msg} — 可在任务中心重试', { msg }))
              window.dispatchEvent(new CustomEvent('dam:library-changed'))
              return {
                ...t,
                status: 'error',
                error: msg,
                failed: outcome.failed ?? 0,
                failedItems: items,
              }
            }),
          )
        }
      } catch {
        /* 后端瞬断：下一轮重试，不打断任务列表 */
      } finally {
        inflight.current = false
      }
    }

    void tick()
    const timer = window.setInterval(tick, POLL_MS)
    return () => {
      stop = true
      window.clearInterval(timer)
    }
  }, [hasRunning, toast])

  const startImport = useCallback(
    (batchId: number, title: string, detail: string) => {
      const id = nextId++
      setTasks((prev) =>
        [
          {
            id,
            kind: 'import' as TaskKind,
            title,
            detail,
            current: 0,
            total: 0,
            status: 'running' as TaskStatus,
            startedAt: Date.now(),
            batchId,
          },
          ...prev,
        ].slice(0, 20),
      )
      return id
    },
    [],
  )

  // ---- W3：嵌入任务真实轮询（后端单一活动任务；所有 embed 行共用同一后端状态） ----
  const hasRunningEmbed = tasks.some((t) => t.kind === 'embed' && t.status === 'running')

  useEffect(() => {
    if (!hasRunningEmbed) return
    let stop = false
    let inflightEmbed = false

    const tick = async () => {
      if (stop || inflightEmbed) return
      inflightEmbed = true
      try {
        const st = await api.embedStatus()
        if (stop) return
        const p = st.progress
        if (p) {
          setTasks((prev) =>
            prev.map((t) =>
              t.kind === 'embed' && t.status === 'running'
                ? { ...t, current: p.index, total: p.total || t.total }
                : t,
            ),
          )
        }
        if (!st.running) {
          const rep = st.report
          const err = st.lastError
          setTasks((prev) =>
            prev.map((t) => {
              if (t.kind !== 'embed' || t.status !== 'running') return t
              if (err && !rep) {
                toast('error', tGlobal('{title} 失败', { title: t.title }), err)
                return { ...t, status: 'error', error: err }
              }
              if (!rep) return t
              window.dispatchEvent(new CustomEvent('dam:library-changed'))
              if (rep.cancelled) {
                toast(
                  'info',
                  tGlobal('{title} 已取消', { title: t.title }),
                  tGlobal('已处理 {n} 项，可重跑续建', { n: rep.processed }),
                )
                return { ...t, status: 'cancelled', current: rep.processed }
              }
              if (rep.failed > 0) {
                const msg = tGlobal('{n} 张失败（{first}）', {
                  n: rep.failed,
                  first: rep.errors[0] ?? tGlobal('见设置页失败列表'),
                })
                toast('error', tGlobal('{title} 部分失败', { title: t.title }), msg)
                return { ...t, status: 'error', current: rep.done, error: msg }
              }
              toast(
                'success',
                tGlobal('{title} 完成', { title: t.title }),
                tGlobal('成功 {n} 张 · 向量已入库（{model}）', { n: rep.done, model: rep.model }),
              )
              return { ...t, status: 'done', current: rep.done, total: rep.done || t.total }
            }),
          )
        }
      } catch {
        /* 后端瞬断：下一轮重试 */
      } finally {
        inflightEmbed = false
      }
    }

    void tick()
    const timer = window.setInterval(tick, POLL_MS)
    return () => {
      stop = true
      window.clearInterval(timer)
    }
  }, [hasRunningEmbed, toast])

  const startEmbed = useCallback((title: string, detail: string) => {
    const id = nextId++
    setTasks((prev) =>
      [
        {
          id,
          kind: 'embed' as TaskKind,
          title,
          detail,
          current: 0,
          total: 0,
          status: 'running' as TaskStatus,
          startedAt: Date.now(),
        },
        ...prev,
      ].slice(0, 20),
    )
    return id
  }, [])

  // ---- W4：打标任务真实轮询（模板 / 自定义两条队列同源 /api/tag/status） ----
  const hasRunningTag = tasks.some((t) => t.kind === 'tag' && t.status === 'running')

  useEffect(() => {
    if (!hasRunningTag) return
    let stop = false
    let inflightTag = false

    const tick = async () => {
      if (stop || inflightTag) return
      inflightTag = true
      try {
        const st = await api.tagStatus()
        if (stop) return
        setTasks((prev) =>
          prev.map((t) => {
            if (t.kind !== 'tag' || t.status !== 'running') return t
            const q = t.tagVariant === 'custom' ? st.custom : st.template
            return q.progress
              ? { ...t, current: q.progress.index, total: q.progress.total || t.total }
              : t
          }),
        )
        if (!st.template.running || !st.custom.running) {
          setTasks((prev) =>
            prev.map((t) => {
              if (t.kind !== 'tag' || t.status !== 'running') return t
              const q = t.tagVariant === 'custom' ? st.custom : st.template
              if (q.running) return t // 另一条队列还在跑
              const rep = q.report
              const err = q.lastError
              if (err && !rep) {
                toast('error', tGlobal('{title} 失败', { title: t.title }), err)
                return { ...t, status: 'error', error: err }
              }
              if (!rep) return t
              window.dispatchEvent(new CustomEvent('dam:library-changed'))
              if (rep.cancelled) {
                toast(
                  'info',
                  tGlobal('{title} 已取消', { title: t.title }),
                  tGlobal('已处理 {n} 项，可续跑', { n: rep.processed }),
                )
                return { ...t, status: 'cancelled', current: rep.processed }
              }
              if (rep.failed > 0) {
                const msg = tGlobal('{n} 张失败（{first}）', {
                  n: rep.failed,
                  first: rep.errors[0] ?? tGlobal('见打标任务页失败列表'),
                })
                toast('error', tGlobal('{title} 部分失败', { title: t.title }), msg)
                return { ...t, status: 'error', current: rep.done, error: msg }
              }
              const skippedNote = rep.skipped > 0 ? tGlobal(' · 跳过 {n}', { n: rep.skipped }) : ''
              // E5 确认流：结果不直接入库，提示去确认页
              toast(
                'success',
                tGlobal('{title} 完成', { title: t.title }),
                tGlobal('{n} 张结果待确认入库{skipped}', { n: rep.done, skipped: skippedNote }),
              )
              void api
                .tagStatus()
                .then((st) => {
                  if (st.review.assets > 0) {
                    toast(
                      'info',
                      tGlobal('打标结果待确认'),
                      tGlobal('{n} 张在确认页等你核对', { n: st.review.assets }),
                    )
                  }
                })
                .catch(() => {})
              return { ...t, status: 'done', current: rep.done, total: rep.done || t.total }
            }),
          )
        }
      } catch {
        /* 后端瞬断：下一轮重试 */
      } finally {
        inflightTag = false
      }
    }

    void tick()
    const timer = window.setInterval(tick, POLL_MS)
    return () => {
      stop = true
      window.clearInterval(timer)
    }
  }, [hasRunningTag, toast])

  const startTag = useCallback(
    (variant: 'template' | 'custom', title: string, detail: string) => {
      const id = nextId++
      setTasks((prev) =>
        [
          {
            id,
            kind: 'tag' as TaskKind,
            title,
            detail,
            current: 0,
            total: 0,
            status: 'running' as TaskStatus,
            startedAt: Date.now(),
            tagVariant: variant,
          },
          ...prev,
        ].slice(0, 20),
      )
      return id
    },
    [],
  )

  const cancel = useCallback(
    (id: number) => {
      const t = tasks.find((x) => x.id === id)
      if (t?.kind === 'embed') {
        void (async () => {
          try {
            await api.embedCancel()
          } catch (e) {
            toast('error', tGlobal('取消失败'), e instanceof Error ? e.message : String(e))
          }
        })()
        return
      }
      if (t?.kind === 'tag') {
        const call = t.tagVariant === 'custom' ? api.tagCustomCancel() : api.tagCancel()
        void call.catch((e: unknown) =>
          toast('error', tGlobal('取消失败'), e instanceof Error ? e.message : String(e)),
        )
        return
      }
      void (async () => {
        try {
          await api.importCancel()
        } catch {
          toast('error', tGlobal('取消失败'), tGlobal('后端无运行中的批次或连接失败'))
        }
      })()
      // 不立刻置 cancelled：下一轮轮询读到 outcome 后归档（保持后端事实源）
    },
    [tasks, toast],
  )

  const retry = useCallback(
    (id: number) => {
      void (async () => {
        const t = tasks.find((x) => x.id === id)
        if (!t) return
        if (t.kind === 'embed') {
          try {
            await api.embedRetryFailed()
            await api.embedStart('pending')
            setTasks((prev) =>
              prev.map((x) =>
                x.id === id ? { ...x, status: 'running', current: 0, error: undefined } : x,
              ),
            )
          } catch (e) {
            toast('error', tGlobal('重试失败'), e instanceof Error ? e.message : String(e))
          }
          return
        }
        if (t.kind === 'tag') {
          try {
            if (t.tagVariant === 'custom') {
              // 自定义：重置失败 + 续跑最近跑过的任务（无则第一个启用任务）
              const { reset } = await api.tagCustomRetryFailed()
              const list = (await api.tagTasks()).items
              const target =
                list
                  .filter((x) => x.enabled)
                  .sort((a, b) => (b.lastRunAt ?? 0) - (a.lastRunAt ?? 0))[0] ?? null
              if (!target) {
                toast('info', tGlobal('没有可续跑的任务'), tGlobal('请到打标任务页创建自定义任务'))
                return
              }
              await api.tagTaskRun(target.id, 'resume')
              setTasks((prev) =>
                prev.map((x) =>
                  x.id === id
                    ? { ...x, status: 'running', current: 0, error: undefined }
                    : x,
                ),
              )
              void reset
            } else {
              await api.tagRetryFailed()
              await api.tagStart('pending')
              setTasks((prev) =>
                prev.map((x) =>
                  x.id === id ? { ...x, status: 'running', current: 0, error: undefined } : x,
                ),
              )
            }
          } catch (e) {
            toast('error', tGlobal('重试失败'), e instanceof Error ? e.message : String(e))
          }
          return
        }
        if (t.batchId === undefined) return
        try {
          await api.importResume(t.batchId)
          setTasks((prev) =>
            prev.map((x) => (x.id === id ? { ...x, status: 'running', current: 0, error: undefined } : x)),
          )
        } catch (e) {
          toast('error', tGlobal('续跑失败'), e instanceof Error ? e.message : String(e))
        }
      })()
    },
    [tasks, toast],
  )

  /** 只重跑本批失败项：走 /api/import/retry-failed（不重扫目录），另起一条任务行，
   *  原失败行保留作历史（重复点击会被「该批次正在跑」挡住）。 */
  const retryFailed = useCallback(
    (id: number) => {
      void (async () => {
        const t = tasks.find((x) => x.id === id)
        if (!t || t.kind !== 'import' || t.batchId === undefined) return
        const paths = (t.failedItems ?? []).map((f) => f.path)
        if (paths.length === 0) {
          toast('info', tGlobal('没有可重试的失败项'), tGlobal('失败明细为空时可用「重试任务」重跑整批'))
          return
        }
        if (tasks.some((x) => x.status === 'running' && x.batchId === t.batchId)) {
          toast('info', tGlobal('该批次正在跑'), tGlobal('等当前任务结束后再重试失败项'))
          return
        }
        try {
          await api.importRetryFailed(t.batchId, paths)
          startImport(
            t.batchId,
            tGlobal('{title}（重试 {n} 张失败项）', { title: t.title, n: paths.length }),
            tGlobal('只重跑上次失败的 {n} 张，不重扫目录', { n: paths.length }),
          )
        } catch (e) {
          toast('error', tGlobal('重试失败项失败'), e instanceof Error ? e.message : String(e))
        }
      })()
    },
    [tasks, toast, startImport],
  )

  const dismiss = useCallback((id: number) => {
    setTasks((prev) => prev.filter((t) => t.id !== id))
  }, [])
  const api2 = useMemo<TaskApi>(
    () => ({
      tasks,
      runningCount: tasks.filter((t) => t.status === 'running').length,
      startImport,
      startEmbed,
      startTag,
      cancel,
      retry,
      retryFailed,
      dismiss,
    }),
    [tasks, startImport, startEmbed, startTag, cancel, retry, retryFailed, dismiss],
  )

  return <TaskCtx.Provider value={api2}>{children}</TaskCtx.Provider>
}

export function useTasks(): TaskApi {
  const ctx = useContext(TaskCtx)
  if (!ctx) throw new Error('useTasks must be used within TaskCenterProvider')
  return ctx
}

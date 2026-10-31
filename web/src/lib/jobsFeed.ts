/**
 * DM-B2（KI-37）：处理记录 L1 的**取数合并**（视图层合并，存储不合并）。
 *
 * 三个来源各拉各的，合并成同一份 `UnifiedJob[]`：
 *  - `/api/compliance/jobs`：检查 / 处理（每次执行一行，带 `totalAssets` + `verdictCounts`）；
 *  - 本地任务表（`useTasks`）：导入 / 打标 / 去重 / 嵌入 / 导出进行中；
 *  - `/api/import/batches` + `/api/export/status`：跨会话回看的导入批次与导出报告。
 *
 * 运行中的 3 秒追一次（不空转：没有任何进行中就不轮询）。
 */
import { tGlobal } from '../i18n'
import { useCallback, useEffect, useMemo, useState } from 'react'

import { api, type ComplianceJobView, type ExportStatus, type ImportBatchRecord } from './api'
import {
  complianceJobToRow,
  exportToRow,
  importBatchToRow,
  mergeJobs,
  taskToRow,
  type UnifiedJob,
} from './jobs'
import { useTasks } from './tasks'

const COMPLIANCE_PAGE = 20
const POLL_MS = 3000

export interface JobsFeed {
  jobs: UnifiedJob[]
  loading: boolean
  error: string | null
  refresh: () => void
}

export function useJobsFeed(): JobsFeed {
  const { tasks } = useTasks()
  const [runs, setRuns] = useState<ComplianceJobView[]>([])
  const [batches, setBatches] = useState<ImportBatchRecord[]>([])
  const [exportStatus, setExportStatus] = useState<ExportStatus | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    const [runsRes, batchesRes, exportRes] = await Promise.allSettled([
      api.complianceJobs({ limit: COMPLIANCE_PAGE }),
      api.importBatches(),
      api.exportStatus(),
    ])
    const errs: string[] = []
    if (runsRes.status === 'fulfilled') setRuns(runsRes.value.jobs)
    else errs.push(`检查与处理：${runsRes.reason instanceof Error ? runsRes.reason.message : String(runsRes.reason)}`)
    if (batchesRes.status === 'fulfilled') setBatches(batchesRes.value)
    else errs.push(`导入批次：${batchesRes.reason instanceof Error ? batchesRes.reason.message : String(batchesRes.reason)}`)
    if (exportRes.status === 'fulfilled') setExportStatus(exportRes.value)
    else errs.push(`导出：${exportRes.reason instanceof Error ? exportRes.reason.message : String(exportRes.reason)}`)
    setError(errs.length ? errs.join(tGlobal('；')) : null)
    setLoading(false)
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const anyRunning = useMemo(
    () => runs.some((r) => r.status === 'running') || !!exportStatus?.running || tasks.some((t) => t.status === 'running'),
    [runs, exportStatus, tasks],
  )
  useEffect(() => {
    if (!anyRunning) return
    const timer = window.setInterval(() => void load(), POLL_MS)
    return () => window.clearInterval(timer)
  }, [anyRunning, load])

  const jobs = useMemo(() => {
    const rows: UnifiedJob[] = [
      ...runs.map(complianceJobToRow),
      ...batches.map(importBatchToRow),
      ...tasks.map(taskToRow),
    ]
    if (exportStatus) {
      const row = exportToRow(exportStatus)
      if (row) rows.push(row)
    }
    return mergeJobs(rows)
  }, [runs, batches, tasks, exportStatus])

  return { jobs, loading, error, refresh: () => void load() }
}

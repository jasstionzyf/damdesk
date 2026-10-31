import { useCallback, useEffect, useRef, useState } from 'react'
import { useLocation, useNavigate } from 'react-router-dom'
import { useTasks } from '../lib/tasks'
import { useToast } from '../lib/toast'
import { useOverlay } from '../lib/modal'
import {
  api,
  DEFAULT_TAG_MAX_TOKENS,
  formatSize,
  type AppInfo,
  type EmbedStatus,
  type ExifSettings as ExifSettingsDto,
  type ExifStatus,
  type InferenceResponse,
  type InferenceSettingsDto,
  type LibraryStats,
  type ModelDownloadStatus,
  type ModelsView,
  type OcrStatusView,
  type SearchStatus,
  type TagProviderPresetDto,
  type VectorModelInfo,
} from '../lib/api'
import { Button } from '../components/Button'
import { useCapabilities } from '../lib/caps'
import { PathLink } from '../components/PathLink'
import { DensityControl, DEFAULT_THUMB, DENSITY_PREF_KEY, clampThumb } from '../components/DensityControl'
import { LOCALES, tGlobal, useI18n, type LocaleCode } from '../i18n'

/**
 * 界面语言（i18n-A 基座，卡 task）。
 *
 * 分区标题硬编码双语「语言 / Language」—— 语言选择器本身就是翻译能力的唯一例外：
 * 在看不懂当前语言时，用户仍要能找到这个入口（W3C 本地化惯例）。
 * 切换经 LocaleProvider：localStorage 持久化 + document.lang 双写，立即生效无重启。
 */
function LanguageSettings() {
  const { locale, setLocale, t } = useI18n()
  return (
    <section className="rounded-lg border border-line bg-panel p-4" data-testid="settings-language">
      <h2 className="text-sm font-semibold">语言 / Language</h2>
      <div className="mt-3 flex items-center gap-3">
        <select
          value={locale}
          onChange={(e) => setLocale(e.target.value as LocaleCode)}
          className="rounded border border-line bg-panel-2 px-2 py-1.5 text-sm"
          aria-label="界面语言 / Interface language"
          data-testid="settings-language-select"
        >
          {LOCALES.map((l) => (
            <option key={l.code} value={l.code}>
              {l.label}
            </option>
          ))}
        </select>
        <span className="text-xs text-ink-dim" data-testid="settings-language-note">
          {t('切换立即生效，无需重启。')}
        </span>
      </div>
    </section>
  )
}

/**
 * KI-14：网格密度档位（S/M/L）从图库工具条移到这里（设置页 → 外观）。
 *
 * 口径：
 *  - 只写「默认偏好」（localStorage），不碰 URL；图库链接自带 `?thumb=` 时仍以链接为准（旧深链继续生效）；
 *  - 用的是同一个 `DensityControl`（同一能力只允许一份实现），不另写副本；
 *  - 命令面板「网格密度」是同一偏好的键盘入口。
 */
const DENSITY_PREF_LABEL = 'dam.gallery.thumb'

function AppearanceSettings() {
  const { t } = useI18n()
  const [thumb, setThumb] = useState(() => {
    try {
      const raw = localStorage.getItem(DENSITY_PREF_KEY)
      return raw ? clampThumb(Number(raw)) : DEFAULT_THUMB
    } catch {
      return DEFAULT_THUMB
    }
  })
  const onChange = (v: number) => {
    const px = clampThumb(v)
    setThumb(px)
    try {
      localStorage.setItem(DENSITY_PREF_KEY, String(px))
    } catch {
      /* 存储不可用：本次会话内仍生效（组件状态），下次打开回落默认档 */
    }
  }
  return (
    <section className="rounded-lg border border-line bg-panel p-4" data-testid="settings-appearance">
      <h2 className="text-sm font-semibold">{t('外观')}</h2>
      <p className="mt-1 text-xs leading-5 text-ink-dim">
        {t('网格密度 = 缩略图最小列宽（列数由窗口宽度自动算）。这里改的是默认档位；图库链接自带')}
        <code className="mx-1 rounded bg-panel-2 px-1">?thumb=</code>
        {t('时以链接为准（旧深链继续生效）。键盘用户也可用命令面板「网格密度」循环切档。')}
      </p>
      <div className="mt-3 flex items-center gap-3">
        <DensityControl thumb={thumb} onChange={onChange} />
      </div>
      <p className="mt-2 text-[11px] text-ink-dim/80" data-testid="settings-appearance-note">
        {t('偏好键：{key}（只在本机生效）', { key: DENSITY_PREF_LABEL })}
      </p>
    </section>
  )
}

/**
 * KI-10：库根列表（设置页视角）
 *
 * 每个库根都是磁盘路径 → 用同一个 `<PathLink mode="open">` 渲染（桌面形态可点开文件夹；
 * 容器形态无按钮、只给复制路径 + 人话原因，形态门控在组件内部）。只读根显式标注 + 原始 code，
 * 不把「不可写」藏起来 —— 否则用户会以为移动/复制失败是 bug。
 */
function LibraryRoots() {
  const { t } = useI18n()
  const caps = useCapabilities()
  const roots = caps?.roots ?? []
  return (
    <section className="rounded-lg border border-line bg-panel p-4" data-testid="settings-library-roots">
      <h2 className="text-sm font-semibold">{t('库根')}</h2>
      <p className="mt-1 text-xs leading-5 text-ink-dim">
        {t('库根是资料库在本机的落脚点（只有可写根能接收移动 / 复制入库）。路径可点：直接在系统文件管理器中打开。')}
      </p>
      {roots.length === 0 ? (
        <p className="mt-2 text-xs text-ink-dim">{caps ? t('当前形态没有可枚举的库根') : t('读取中…')}</p>
      ) : (
        <ul className="mt-2 space-y-2" data-testid="settings-library-roots-list">
          {roots.map((r) => (
            <li key={r.path} className="rounded border border-line bg-panel-2 px-2.5 py-2 text-xs">
              <div className="flex flex-wrap items-center gap-2">
                <span className={r.writable ? 'text-emerald-400' : 'text-amber-300'}>
                  {r.writable ? t('可写') : t('只读')}
                </span>
                {!r.writable && r.code && <span className="text-ink-dim">{t('（{code}）', { code: r.code })}</span>}
              </div>
              <PathLink path={r.path} mode="open" className="mt-1" />
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

/**
 * 模型文件（可选下载）—— 设置页分区（卡 task）。
 *
 * 为什么要有它：`lama-fp32`（局部重绘 208MB）不随安装包内置，而在此之前**没有任何下载入口**，
 * 只能靠人工把 onnx 拖进模型目录 —— A5 局部重绘在真机上等于永远缺模型（判定能跑、修复不能跑）。
 *
 * 口径：
 *  - 只显示 `downloadable` 的 manifest（未内置档）；内置模型（随安装包装）不在这里出现，
 *    免得同一个模型两处展示、用户以为还要再下一遍；
 *  - 缺件时给出**引擎实际读取的绝对路径**，能下就点下载，下不了也能照路径手放；
 *  - 进度/失败来自后端常驻状态（/api/models/download/status），失败如实显示原因 + 可重试，
 *    绝不因为「点过下载」就把状态显示成可用（就绪只认文件在盘）。
 */
/** 模型展示名/用途（i18n-D：函数取词，不在模块顶层缓存译文——locale 会变） */
function modelLabel(name: string): { title: string; usage: string } {
  if (name === 'lama-fp32') {
    return {
      title: tGlobal('局部重绘（big-lama）'),
      usage: tGlobal('合规修复的 inpaint 步骤（去水印 / 去杂物）用它；桌面版本地推理，图片不出本机'),
    }
  }
  return { title: name, usage: '' }
}

function ModelFilesSettings() {
  const { t } = useI18n()
  const { toast } = useToast()
  const [view, setView] = useState<ModelsView | null>(null)
  const [viewErr, setViewErr] = useState<string | null>(null)
  const [status, setStatus] = useState<ModelDownloadStatus | null>(null)
  const [busy, setBusy] = useState<string | null>(null)

  const refresh = useCallback(() => {
    api
      .models()
      .then((v) => {
        setView(v)
        setViewErr(null)
      })
      .catch((e) => {
        setView(null)
        setViewErr(e instanceof Error ? e.message : String(e))
      })
    api.modelDownloadStatus().then(setStatus).catch(() => setStatus(null))
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  // 只在下载进行中轮询（设置页不空转）
  const running = status?.running ?? false
  useEffect(() => {
    if (!running) return
    const t = window.setInterval(refresh, 1000)
    return () => window.clearInterval(t)
  }, [running, refresh])

  const start = (name: string) =>
    void (async () => {
      setBusy(name)
      try {
        const r = await api.modelDownload(name)
        toast('info', t('开始下载模型'), `${name} → ${r.root}`)
      } catch (e) {
        toast('error', t('下载未启动'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(null)
        refresh()
      }
    })()

  const rows = (view?.models ?? []).filter((m) => m.downloadable)

  return (
    <section id="models" className="rounded-lg border border-line bg-panel p-4" data-testid="models-section">
      <h2 className="text-sm font-semibold">{t('模型文件（可选下载）')}</h2>
      <p className="mt-1 text-xs leading-5 text-ink-dim">
        {t(
          '随安装包内置的模型不在这里（已就位）。下面这些按体积裁决不内置，缺了对应能力就直接不可用 —— 点「下载」由后端拉取并做 sha256 校验，下载后立刻生效，无需重启。',
        )}
      </p>

      {viewErr && (
        <p className="mt-2 text-xs text-red-400" data-testid="models-error">
          {t('读取模型清单失败（{err}）', { err: viewErr })}
        </p>
      )}
      {view?.note && (
        <p className="mt-2 text-xs text-amber-300" data-testid="models-note">
          {view.note}
        </p>
      )}
      {view && (
        <p className="mt-2 text-[11px] leading-5 text-ink-dim" data-testid="models-root">
          {t('模型目录')} <code className="font-mono break-all">{view.root}</code>
          {view.baseUrl ? (
            <>
              {' '}
              · {t('下载源')} <code className="font-mono break-all">{view.baseUrl}</code>
            </>
          ) : null}
        </p>
      )}

      {rows.length === 0 && !viewErr && (
        <p className="mt-2 text-xs text-ink-dim" data-testid="models-empty">
          {view ? t('没有需要额外下载的模型。') : t('读取中…')}
        </p>
      )}

      <ul className="mt-2 space-y-2">
        {rows.map((m) => {
          const label = modelLabel(m.name)
          const isThis = status?.model === m.name
          const downloading = running && isThis
          const failed = isThis && !running && status?.status === 'failed'
          const pct =
            downloading && (status?.totalBytes ?? 0) > 0
              ? Math.min(100, Math.round(((status?.doneBytes ?? 0) / (status!.totalBytes || 1)) * 100))
              : 0
          return (
            <li
              key={m.name}
              className="rounded border border-line bg-panel-2 px-3 py-2"
              data-testid={`model-row-${m.name}`}
            >
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-sm font-medium">{label.title}</span>
                <code className="font-mono text-[11px] text-ink-dim">{m.name}</code>
                <span className="text-[11px] text-ink-dim">{formatSize(m.totalBytes)}</span>
                <span
                  className={`text-[11px] ${m.ready ? 'text-emerald-400' : 'text-amber-300'}`}
                  data-testid={`model-state-${m.name}`}
                >
                  {m.ready ? t('已就绪') : downloading ? t('下载中 {pct}%', { pct }) : t('未下载')}
                </span>
                {!m.ready && (
                  <Button
                    type="button"
                    data-testid={`model-download-${m.name}`}
                    disabled={busy === m.name || running}
                    onClick={() => start(m.name)}
                    variant="secondary"
                  >
                    {downloading
                      ? t('下载中…')
                      : busy === m.name
                        ? t('启动中…')
                        : t('下载（{size}）', { size: formatSize(m.totalBytes) })}
                  </Button>
                )}
                {failed && (
                  <Button
                    type="button"
                    data-testid={`model-retry-${m.name}`}
                    disabled={running}
                    onClick={() => start(m.name)}
                    variant="secondary"
                  >
                    {t('重试')}
                  </Button>
                )}
              </div>
              {label.usage && <p className="mt-1 text-[11px] leading-5 text-ink-dim">{label.usage}</p>}

              {downloading && (
                <div className="mt-2" data-testid={`model-progress-${m.name}`}>
                  <div className="h-1.5 w-full overflow-hidden rounded bg-line">
                    <div className="h-full bg-accent transition-all" style={{ width: `${pct}%` }} />
                  </div>
                  <p className="mt-1 text-[11px] text-ink-dim tabular-nums">
                    {formatSize(status?.doneBytes ?? 0)} / {formatSize(m.totalBytes)}
                    {status && status.totalFiles > 1
                      ? ` · ${t('文件 {done}/{total}', { done: status.completedFiles, total: status.totalFiles })}`
                      : ''}
                  </p>
                </div>
              )}

              {failed && (
                <p
                  className="mt-2 rounded border border-red-600/40 bg-red-950/20 px-2 py-1.5 text-[11px] leading-5 text-red-300"
                  data-testid={`model-error-${m.name}`}
                >
                  {t('下载失败：{err}（可点「重试」；也可把文件按下面的路径手工放置）', {
                    err: status?.error || t('未知原因'),
                  })}
                </p>
              )}

              {!m.ready && m.missingPaths.length > 0 && (
                <div className="mt-2 text-[11px] leading-5 text-ink-dim" data-testid={`model-missing-${m.name}`}>
                  {t('缺失文件（后端实际读取位置）：')}
                  <ul className="mt-1 list-disc pl-4 font-mono break-all">
                    {m.missingPaths.map((p) => (
                      <li key={p}>{p}</li>
                    ))}
                  </ul>
                </div>
              )}
            </li>
          )
        })}
      </ul>
    </section>
  )
}

/**
 * 文字识别（OCR）—— 设置页分区（卡 task：从打标任务页整块迁入，一能力一处）
 *
 * 只管两件事：①引擎状态（模型三件 det/rec/dict 是否在盘，后端 /api/ocr/status 常驻返回，
 * 缺件给绝对路径，放好即用不重启）；②批量识别队列（增量 / 全库重跑 / 取消 / 进度 / 上轮汇总）。
 * 推理实现与模型加载属 engine-ocr 领地，本页不动。
 */
function OcrSettings({ landed }: { landed?: boolean }) {
  const { t } = useI18n()
  const { toast } = useToast()
  const [ocr, setOcr] = useState<OcrStatusView | null>(null)
  const [ocrErr, setOcrErr] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const refresh = useCallback(() => {
    api
      .ocrStatus()
      .then((v) => {
        setOcr(v)
        setOcrErr(null)
      })
      .catch((e) => {
        // 老后端没有 /api/ocr/status：说清「读不到」，不要一直挂「读取中…」骗人
        setOcr(null)
        setOcrErr(e instanceof Error ? e.message : String(e))
      })
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  // 只在识别进行中轮询（设置页不空转）
  const running = ocr?.running ?? false
  useEffect(() => {
    if (!running) return
    const t = window.setInterval(refresh, 3000)
    return () => window.clearInterval(t)
  }, [running, refresh])

  const start = (mode: 'pending' | 'all') =>
    void (async () => {
      setBusy(true)
      try {
        const r = await api.ocrStart(mode)
        toast('info', mode === 'all' ? t('已开始全库识别') : t('已开始识别'), t('入队 {n} 张', { n: r.enqueued }))
      } catch (e) {
        toast('error', t('启动失败'), e instanceof Error ? e.message : String(e))
      } finally {
        setBusy(false)
        refresh()
      }
    })()

  const notReady = ocr?.modelsReady === false
  // 老后端/异常负载可能没有 stats 字段：读数按 0 展示，不许把整个设置页打崩
  // （task 合并后 journey 实测：ocr 有值但 stats 缺失时 `ocr?.stats.done` 仍会炸）
  const st = ocr?.stats
  const tiles: [string, number, string?][] = [
    [t('已完成'), st?.done ?? 0],
    [t('待识别'), st?.noOcrAssets ?? 0, t('增量入队范围')],
    [t('排队中'), st?.pending ?? 0],
    [t('失败'), st?.failed ?? 0],
  ]

  return (
    <section
      id="ocr"
      className={`rounded-lg border bg-panel p-4 ${landed ? 'border-accent ring-1 ring-accent/40' : 'border-line'}`}
      data-testid="ocr-section"
      data-landed={landed ? 'true' : undefined}
    >
      <h2 className="text-sm font-semibold">{t('文字识别（OCR）')}</h2>
      <p className="mt-1 text-xs leading-5 text-ink-dim">
        {t(
          'PP-OCRv6_medium 本地推理（det + rec + dict，约 132MB，Apache-2.0）· 行文本入库 · 检索时按文字命中加权。 图片不出本机。',
        )}
      </p>

      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-ink-dim" data-testid="ocr-readiness">
        <span>
          {t('可用性：')}
          {ocrErr ? (
            <span className="text-red-400">{t('读取失败（{err}）', { err: ocrErr })}</span>
          ) : ocr == null ? (
            t('读取中…')
          ) : ocr.modelsReady ? (
            <span className="text-emerald-400">{t('就绪（已启用）')}</span>
          ) : (ocr.missingModelFiles?.length ?? 0) > 0 ? (
            <span className="text-amber-300">
              {t('未就绪，缺 {n} 件模型文件（识别必失败）', { n: ocr.missingModelFiles!.length })}
            </span>
          ) : (
            <span className="text-amber-300">{t('未就绪（模型文件缺失，后端未给出具体路径，见下方）')}</span>
          )}
        </span>
        <span>{t('语言：中文 + 英文（内置 18708 字字典，无需另配）')}</span>
      </div>

      <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-4">
        {tiles.map(([label, v, hint]) => (
          <div key={label} className="rounded border border-line bg-panel-2 px-2 py-2">
            <p className="text-lg font-semibold tabular-nums">{v}</p>
            <p className="text-[11px] text-ink-dim">
              {label}
              {hint ? t('（{hint}）', { hint }) : ''}
            </p>
          </div>
        ))}
      </div>

      {notReady && (
        <div
          data-testid="ocr-models-missing"
          className="mt-3 rounded border border-amber-500/50 bg-amber-500/5 px-3 py-2 text-[11px] leading-relaxed text-amber-200"
        >
          <div className="text-sm font-semibold text-amber-300">
            {t('OCR 模型未就绪，点「识别」会直接失败（不是网络问题）')}
          </div>
          <div className="mt-1">
            {t('PP-OCRv6_medium 三件（det / rec / dict，约 132MB，Apache-2.0）不随安装包内置，放到下列目录即可（这是后端实际读取的绝对路径）：')}
          </div>
          <ul className="mt-1 list-disc pl-4 font-mono break-all">
            {(ocr?.missingModelFiles ?? []).map((p) => (
              <li key={p}>{p}</li>
            ))}
            {(ocr?.missingModelFiles ?? []).length === 0 && (
              <li className="text-amber-300/80">
                {t('（后端未返回具体路径：模型根可能未配置，先看「诊断」里的模型目录）')}
              </li>
            )}
          </ul>
          <div className="mt-1">
            {t(
              '来源：PaddlePaddle 官方 ONNX 仓的 ppocrv6-medium-det / ppocrv6-medium-rec（inference.onnx + ppocrv6_dict.txt）。放好后直接重试，无需重启。',
            )}
          </div>
        </div>
      )}

      <div className="mt-3 flex flex-wrap items-center gap-2">
        <Button
          type="button"
          data-testid="ocr-start-pending"
          disabled={busy || running || notReady}
          onClick={() => start('pending')}
          variant="secondary"
        >
          {running ? t('识别中…') : t('识别未处理的')}
        </Button>
        <Button
          type="button"
          data-testid="ocr-start-all"
          disabled={busy || running || notReady}
          onClick={() => {
            if (window.confirm(t('全库重跑会覆盖全部已识别文本，继续？'))) start('all')
          }}
          variant="secondary"
        >
          {t('全库重跑')}
        </Button>
        <Button
          type="button"
          data-testid="ocr-cancel"
          disabled={!running}
          onClick={() => void api.ocrCancel().then(() => toast('info', t('已请求取消'), ''))}
          variant="secondary"
        >
          {t('取消')}
        </Button>
        {ocr?.running && ocr.progress && (
          <span className="text-xs text-ink-dim">
            {t('进度：{index}/{total} · 成功 {done} · 失败 {failed}', {
              index: ocr.progress.index,
              total: ocr.progress.total,
              done: ocr.progress.doneCount,
              failed: ocr.progress.failedCount,
            })}
          </span>
        )}
        {ocr?.report && (
          <span className="text-xs text-ink-dim">
            {t('上轮：处理 {processed} · 成功 {done} · 失败 {failed} · 跳过 {skipped}{cancelled}', {
              processed: ocr.report.processed,
              done: ocr.report.done,
              failed: ocr.report.failed,
              skipped: ocr.report.skipped,
              cancelled: ocr.report.cancelled ? t(' · 已取消') : '',
            })}
          </span>
        )}
        {ocr?.error && <span className="text-xs text-red-500">{ocr.error}</span>}
      </div>
    </section>
  )
}

/**
 * 诊断（折叠区，默认收起）—— 只读体检：向量表 / 行数 / 当前模型 / 库大小 / 版本。
 *
 * 折叠态**一个诊断请求都不发**：/api/search/status、/api/library/stats、/api/app-info
 * 只在展开时才读（卡 task 验收项：收起时断网不影响设置页其余分区）。
 * 可编辑项（库根 / provider / EXIF / OCR）留在上方，本区不承载任何写操作。
 */
function Diagnostics() {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const [search, setSearch] = useState<SearchStatus | null>(null)
  const [stats, setStats] = useState<LibraryStats | null>(null)
  const [app, setApp] = useState<AppInfo | null>(null)
  const [loaded, setLoaded] = useState(false)
  /**
   * 语义塔（文搜图可用 spec）的模型标识：向量表只列这些。
   * KI-6 裁决「设置界面不展示去重模型信息」→ 判重塔/其它非语义塔的向量表不进这张表；
   * 判断依据是能力（registry 里 modalities 含 text 的 spec 名，可带 `:端点模型` 后缀），
   * 不写死任何塔名。读不到清单时保守不列（宁缺不泄）。
   */
  const [semanticModels, setSemanticModels] = useState<string[] | null>(null)

  const load = useCallback(() => {
    api
      .searchStatus()
      .then(setSearch)
      .catch(() => setSearch(null))
      .finally(() => setLoaded(true))
    api
      .stats()
      .then(setStats)
      .catch(() => setStats(null))
    api
      .appInfo()
      .then(setApp)
      .catch(() => setApp(null))
    api
      .inference()
      .then((r) => setSemanticModels(r.embedders.map((e) => e.name)))
      .catch(() => setSemanticModels(null))
  }, [])

  // 懒加载：展开才读（收起态零请求）
  useEffect(() => {
    if (open) load()
  }, [open, load])

  const tiles: [string, number][] = [
    [t('已嵌入'), search?.semantic.done ?? 0],
    [t('待嵌入'), search?.semantic.remaining ?? 0],
    [t('失败'), search?.semantic.failed ?? 0],
    [t('向量行数'), search?.activeRows ?? 0],
  ]

  /** 只列语义塔的向量表（KI-6：设置界面不出现去重塔的向量表/行数） */
  const vecTables: VectorModelInfo[] = ((search?.models ?? []) as VectorModelInfo[]).filter(
    (m: VectorModelInfo) =>
      (semanticModels ?? []).some(
        (n: string) => m.model === n || m.model.startsWith(`${n}:`),
      ),
  )
  /** 「库内还有其它模型的向量」= 语义塔里非当前活跃的那些（去重塔不算，它不是可选 provider） */
  const otherSemanticVectors = vecTables.some((m: VectorModelInfo) => !m.active)

  return (
    <section className="rounded-lg border border-line bg-panel p-4" data-testid="settings-diagnostics">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="min-w-0">
          <h2 className="text-sm font-semibold">{t('诊断')}</h2>
          <p className="mt-1 text-xs leading-5 text-ink-dim">
            {t('只读体检：向量表 / 行数 / 当前模型 / 库大小 / 版本。默认收起 —— 展开时才读取，不占设置页首屏。')}
          </p>
        </div>
        <Button
          type="button"
          data-testid="diag-toggle"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
          variant="secondary"
        >
          {open ? t('收起') : t('展开诊断')}
        </Button>
      </div>

      {open && (
        <div className="mt-3" data-testid="diag-body">
          <div className="grid grid-cols-1 gap-2 text-xs sm:grid-cols-3">
            <div className="rounded border border-line bg-panel-2 px-2 py-1.5" data-testid="diag-version">
              <span className="text-ink-dim">{t('版本')}</span>{' '}
              <span className="tabular-nums">{app ? `v${app.version}` : '—'}</span>
            </div>
            <div className="rounded border border-line bg-panel-2 px-2 py-1.5" data-testid="diag-library-size">
              <span className="text-ink-dim">{t('库大小')}</span>{' '}
              <span className="tabular-nums">
                {stats ? t('{n} 张 · {size}', { n: stats.assets, size: formatSize(stats.totalBytes) }) : '—'}
              </span>
            </div>
            <div className="rounded border border-line bg-panel-2 px-2 py-1.5" data-testid="diag-active-model">
              <span className="text-ink-dim">{t('当前模型')}</span>{' '}
              <span>{search?.activeModel || t('未配置')}</span>
              <span className="text-ink-dim">{t('（{n} 维）', { n: search?.activeDims || '—' })}</span>
            </div>
          </div>

          {app && (
            <p className="mt-2 text-[11px] leading-5 text-ink-dim" data-testid="diag-paths">
              {t('数据目录')} <code className="font-mono">{app.dataDir}</code> · {t('模型目录')}{' '}
              <code className="font-mono">{app.modelsRoot}</code>
            </p>
          )}

          {stats?.missingStale && (
            <p className="mt-1 text-[11px] text-amber-300/90">
              {t('失联数 {n} 来自上次现场核查，已过期（未实时重查）。', { n: stats.missing })}
            </p>
          )}

          {/*
           * GH-5：判重塔（DINOv3）不随安装包分发（Meta 许可禁止再分发）→ 可选塔。
           * 缺件时这里如实报缺 + 自备指引（后端给落点路径），不许静默把 pHash 层当判重塔结论。
           * 可用时不渲染 —— KI-6「设置界面不展示去重模型信息」仅在可用侧仍然成立。
           */}
          {stats && stats.dedupAvailable === false && (
            <p className="mt-1 text-[11px] leading-5 text-amber-400" data-testid="diag-dedup-unavailable">
              {t('判重不可用，需自备模型（Meta 许可）')}
              {stats.dedupUnavailableReason && (
                <span className="block text-ink-dim">{stats.dedupUnavailableReason}</span>
              )}
            </p>
          )}

          <div className="mt-3 grid grid-cols-2 gap-2 text-center sm:grid-cols-4">
            {tiles.map(([label, v]) => (
              <div key={label} className="rounded border border-line bg-panel-2 px-2 py-2">
                <p className="text-lg font-semibold tabular-nums">{v}</p>
                <p className="text-[11px] text-ink-dim">{label}</p>
              </div>
            ))}
          </div>

          <p className="mt-2 text-xs leading-5 text-ink-dim">
            {search ? (
              search.searchable ? (
                <span className="text-emerald-400">{t('搜索可用')}</span>
              ) : (
                <span className="text-amber-400">{t('尚无向量，请先嵌入')}</span>
              )
            ) : loaded ? (
              <span className="text-red-400">{t('读取向量状态失败：后端未就绪？')}</span>
            ) : (
              t('读取中…')
            )}
            {otherSemanticVectors && (
              <span className="block text-amber-400">
                {t('库内存在其它模型/端点的向量（不同表，不混算）：换 provider 后需重跑嵌入。')}
              </span>
            )}
          </p>

          {search && vecTables.length > 0 && (
            <div className="mt-3" data-testid="diag-vector-tables">
              <p className="text-xs font-semibold text-ink-dim">{t('向量表（每模型一张，禁混算）')}</p>
              <table className="mt-1.5 w-full text-xs">
                <thead className="text-ink-dim">
                  <tr>
                    <th className="text-left font-normal">{t('模型标识')}</th>
                    <th className="text-right font-normal">{t('维度')}</th>
                    <th className="text-right font-normal">{t('行数')}</th>
                    <th className="text-right font-normal">{t('表')}</th>
                  </tr>
                </thead>
                <tbody>
                  {vecTables.map((m: VectorModelInfo) => (
                    <tr key={m.model} className={m.active ? 'text-ink' : 'text-ink-dim'}>
                      <td className="truncate py-0.5">
                        {m.model}
                        {m.active && <span className="ml-1 text-accent">· {t('当前')}</span>}
                      </td>
                      <td className="py-0.5 text-right tabular-nums">{m.dims}</td>
                      <td className="py-0.5 text-right tabular-nums" data-testid={`diag-rows-${m.model}`}>
                        {m.rows}
                      </td>
                      <td className="truncate py-0.5 text-right">{m.table}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}
    </section>
  )
}

/**
 * 设置页（W3）：embed provider 可插拔（R2/R6）。
 * web 构建不含本地 ONNX（local-infer 关闭）→ 只能选 openai_compat 远程端点
 * （dam-ai 自托管 / dashscope / 任意 OpenAI 兼容 base_url），与桌面版同一套配置能力。
 * 另含向量索引任务动作（增量/全库重嵌入、失败重试）；只读诊断（向量表/行数/当前模型/
 * 库大小/版本）收进页面底部「诊断」折叠区（卡 task：可编辑项在上，体检按需展开）。
 */
export function Settings() {
  const { t } = useI18n()
  const { startEmbed } = useTasks()
  const { toast } = useToast()
  const navigate = useNavigate()
  /** 处理插件的实体在右侧 Drawer（`?drawer=packs`），设置页只留入口按钮（归属单一） */
  const packsOverlay = useOverlay('drawer', 'packs')
  const openOverlay = (_name: 'packs') => packsOverlay.openOverlay()

  const [info, setInfo] = useState<InferenceResponse | null>(null)
  const [form, setForm] = useState<InferenceSettingsDto | null>(null)
  const [embed, setEmbed] = useState<EmbedStatus | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [testResult, setTestResult] = useState<string | null>(null)
  const [tagPresets, setTagPresets] = useState<TagProviderPresetDto[]>([])
  const [tagTestResult, setTagTestResult] = useState<string | null>(null)
  const [scanModels, setScanModels] = useState<string[]>([])
  const [exifCfg, setExifCfg] = useState<ExifSettingsDto | null>(null)
  const [exifStatus, setExifStatus] = useState<ExifStatus | null>(null)
  const [failures, setFailures] = useState<
    { jobId: number; assetId: number; model: string; attempts: number; lastError: string }[]
  >([])

  // toast 走 ref：refresh 不能因为「provider 每次 render 换了新函数」而重建，
  // 否则 `useEffect(..., [refresh])` 会随每次 render 重跑 → 请求 + setState 自激循环。
  const toastRef = useRef(toast)
  toastRef.current = toast

  const refresh = useCallback(() => {
    api
      .inference()
      .then((r) => {
        setInfo(r)
        setForm((prev) => prev ?? r.settings)
      })
      .catch((e) => toastRef.current('error', t('读取设置失败'), e instanceof Error ? e.message : String(e)))
    api.embedStatus().then(setEmbed).catch(() => setEmbed(null))
    api.tagPresets().then((r) => setTagPresets(r.presets)).catch(() => setTagPresets([]))
    api
      .embedFailed()
      .then((r) => setFailures(r.items))
      .catch(() => setFailures([]))
    api
      .exifSettings()
      .then(setExifCfg)
      .catch(() => setExifCfg(null))
    api.exifStatus().then(setExifStatus).catch(() => setExifStatus(null))
    // t 只随 locale 变化重建（toast 每渲染换新函数的自激循环不适用于它）
  }, [t])

  useEffect(() => {
    refresh()
  }, [refresh])

  /**
   * 深链落地（卡 task）：打标任务页的「OCR 模型未就绪」提示跳到 `/settings#ocr`，
   * 这里把目标分区滚进视野并加一圈高亮 —— 落地是「一眼看见该分区」，不是只换个页面。
   * hash 变化时重放（同页再点提示也有效）。
   */
  const location = useLocation()
  const [landed, setLanded] = useState('')
  useEffect(() => {
    const anchor = location.hash.replace(/^#/, '')
    if (!anchor) {
      setLanded('')
      return
    }
    const el = document.getElementById(anchor)
    if (!el) return
    el.scrollIntoView?.({ block: 'start' })
    setLanded(anchor)
  }, [location.hash])

  // 嵌入跑完后自动刷新统计
  const running = embed?.running ?? false
  useEffect(() => {
    if (!running) return
    const t = window.setInterval(refresh, 1500)
    return () => window.clearInterval(t)
  }, [running, refresh])

  const save = async () => {
    if (!form) return
    setBusy('save')
    try {
      const r = await api.saveInference(form)
      setForm(r.settings)
      toast('success', t('设置已保存'), t('当前 embedder：{name}', { name: r.settings.active_embedder }))
      setTestResult(null)
      refresh()
    } catch (e) {
      toast('error', t('保存失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  const test = async () => {
    setBusy('test')
    setTestResult(null)
    try {
      const r = await api.testEmbed()
      setTestResult(
        t('连接成功：endpoint 模型 {model} · 向量维度 {dims} · 标识 {key}', {
          model: r.endpointModel,
          dims: r.dims,
          key: r.modelKey,
        }),
      )
      toast('success', t('推理端点可用'), t('{url} → {dims} 维', { url: r.baseUrl, dims: r.dims }))
      refresh()
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e)
      setTestResult(t('连接失败：{err}', { err: msg }))
      toast('error', t('推理端点不可用'), msg)
    } finally {
      setBusy(null)
    }
  }

  const startIndex = async (mode: 'pending' | 'all') => {
    try {
      const r = await api.embedStart(mode)
      if (r.queued === 0) {
        toast('info', t('无需嵌入'), t('所有图片都已有当前模型的向量'))
        return
      }
      startEmbed(mode === 'all' ? t('全库重嵌入') : t('增量嵌入'), t('{n} 张 · {model}', { n: r.queued, model: r.model }))
      toast('info', t('嵌入任务已启动'), t('进度见右上角处理记录'))
    } catch (e) {
      toast('error', t('启动失败'), e instanceof Error ? e.message : String(e))
    }
  }

  const retryFailed = async () => {
    try {
      const r = await api.embedRetryFailed()
      toast('success', t('失败项已重置'), t('{n} 项待重跑（点「增量嵌入」消费）', { n: r.reset }))
      refresh()
    } catch (e) {
      toast('error', t('重试失败'), e instanceof Error ? e.message : String(e))
    }
  }

  const infer = info?.capabilities

  return (
    <div className="min-h-0 flex-1 overflow-y-auto px-6 py-5">
      <div className="mx-auto max-w-3xl space-y-6">
        {/* ---------- 语言（i18n-A：界面语言切换，其它分区的 t() 都依赖这层） ---------- */}
        <LanguageSettings />
        {/* ---------- 外观（KI-14：网格密度档位从图库工具条搬来） ---------- */}
        <AppearanceSettings />
        {/* ---------- 推理引擎（embed provider） ---------- */}
        <section className="rounded-lg border border-line bg-panel p-4">
          <h2 className="text-sm font-semibold">{t('推理引擎（embed provider）')}</h2>
          <p className="mt-1 text-xs leading-5 text-ink-dim">
            {t('文搜图 / 以图搜图用的向量由这里的 provider 生成，向量按模型标识分表存放（禁混算）。')}
            {infer && !infer.localInfer && (
              <span className="mt-1 block text-amber-400">{infer.note}</span>
            )}
          </p>

          {form && info && (
            <div className="mt-4 space-y-3">
              <label className="block">
                <span className="text-xs text-ink-dim">{t('激活的 embedder（registry spec）')}</span>
                <select
                  value={form.active_embedder}
                  onChange={(e) => setForm({ ...form, active_embedder: e.target.value })}
                  className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm"
                >
                  {info.embedders.map((m) => {
                    const disabled = !infer?.localInfer && m.provider.startsWith('onnx_')
                    return (
                      <option key={m.name} value={m.name} disabled={disabled}>
                        {m.name}
                        {t(
                          '（{provider}，{dims}）',
                          {
                            provider: m.provider,
                            dims: m.dims > 0 ? t('{n} 维', { n: m.dims }) : t('维度实测'),
                          },
                        )}
                        {disabled ? t(' — web 不可用') : ''}
                      </option>
                    )
                  })}
                </select>
                {/* 仅图像向量塔（去重塔）不进候选也不做说明：KI-6 裁决「设置界面不展示去重模型信息」。
                    下拉数据源 = 后端 embedding_specs（已按 modalities 含 text 过滤），这里不再兜底渲染。 */}
              </label>

              <div className="grid grid-cols-2 gap-3">
                <label className="col-span-2 block">
                  <span className="text-xs text-ink-dim">{t('base_url（含 /v1）')}</span>
                  <input
                    data-testid="embed-base-url"
                    value={form.embedding.base_url}
                    onChange={(e) =>
                      setForm({ ...form, embedding: { ...form.embedding, base_url: e.target.value } })
                    }
                    placeholder={
                      infer?.localInfer
                        ? t('http://127.0.0.1:8090/v1（自建 vLLM / 云 API 的 /v1 地址）')
                        : t('http://host.docker.internal:8090/v1（仅容器形态默认；云 API 填自己的 /v1 地址）')
                    }
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>
                <label className="block">
                  <span className="text-xs text-ink-dim">{t('endpoint 模型名')}</span>
                  <input
                    value={form.embedding.model}
                    onChange={(e) =>
                      setForm({ ...form, embedding: { ...form.embedding, model: e.target.value } })
                    }
                    placeholder="qwen3-vl-embedding-2b"
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>
                <label className="block">
                  <span className="text-xs text-ink-dim">{t('api_key（自建端点可留空）')}</span>
                  <input
                    value={form.embedding.api_key}
                    onChange={(e) =>
                      setForm({ ...form, embedding: { ...form.embedding, api_key: e.target.value } })
                    }
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>
                <label className="block">
                  <span className="text-xs text-ink-dim">{t('维度（0 = 以端点实测为准）')}</span>
                  <input
                    type="number"
                    value={form.embedding.dims}
                    onChange={(e) =>
                      setForm({ ...form, embedding: { ...form.embedding, dims: Number(e.target.value) } })
                    }
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>
                <label className="block">
                  <span className="text-xs text-ink-dim">{t('超时（秒）')}</span>
                  <input
                    type="number"
                    value={form.embedding.timeout_secs}
                    onChange={(e) =>
                      setForm({
                        ...form,
                        embedding: { ...form.embedding, timeout_secs: Number(e.target.value) },
                      })
                    }
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>
                <label className="col-span-2 block">
                  <span className="text-xs text-ink-dim">{t('代理（留空 = 直连；None = 走环境变量代理）')}</span>
                  <input
                    value={form.embedding.proxy ?? ''}
                    onChange={(e) =>
                      setForm({ ...form, embedding: { ...form.embedding, proxy: e.target.value } })
                    }
                    placeholder={t('留空表示直连')}
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>
              </div>

              <div className="flex items-center gap-2">
                <Button
                  type="button"
                  onClick={() => void save()}
                  disabled={busy !== null}
                  variant="primary"
                >
                  {busy === 'save' ? t('保存中…') : t('保存')}
                </Button>
                <Button
                  type="button"
                  onClick={() => void test()}
                  disabled={busy !== null}
                  variant="secondary"
                >
                  {busy === 'test' ? t('测试中…') : t('测试连接')}
                </Button>
                {testResult && (
                  <span
                    className={`text-xs ${testResult.startsWith(t('连接成功')) ? 'text-emerald-400' : 'text-red-400'}`}
                  >
                    {testResult}
                  </span>
                )}
              </div>
            </div>
          )}
        </section>

        {/* ---------- AI 打标（tag provider，R7/D9） ---------- */}
        <section className="rounded-lg border border-line bg-panel p-4">
          <h2 className="text-sm font-semibold">{t('AI 打标（tag provider）')}</h2>
          <p className="mt-1 text-xs leading-5 text-ink-dim">
            {t(
              '桌面版默认 DeepSeek 多模态 API（api.deepseek.com，Key 在 platform.deepseek.com 申请）；容器版默认 dam-ai /v1/tagging 自托管（vLLM guided decoding）。也可切 dashscope / 智谱 / Ollama / 任意 OpenAI 兼容端点。标签按「provider:模型」标识隔离，换 provider 或模型后旧标签保留，可用「全库重打」显式覆盖。跑批入口在',
            )}{' '}
            <a href="/tag-tasks" className="text-accent hover:underline">
              {t('打标任务页')}
            </a>
            {t('。')}
          </p>

          {form && (
            <div className="mt-4 space-y-3">
              <label className="flex items-center gap-2 text-xs text-ink-dim">
                <input
                  type="checkbox"
                  checked={form.tag.enabled}
                  onChange={(e) =>
                    setForm({ ...form, tag: { ...form.tag, enabled: e.target.checked } })
                  }
                />
                {t('启用 AI 打标')}
              </label>

              <div className="grid grid-cols-2 gap-3">
                <label className="col-span-2 block">
                  <span className="text-xs text-ink-dim">{t('provider 预设')}</span>
                  <select
                    value={form.tag.provider}
                    onChange={(e) => {
                      const p = tagPresets.find((x) => x.id === e.target.value)
                      setForm({
                        ...form,
                        tag: {
                          ...form.tag,
                          provider: e.target.value,
                          // 切预设即预填 base_url / 模型候选（可改）
                          base_url: p?.base_url ?? form.tag.base_url,
                          model: p?.models[0] ?? form.tag.model,
                        },
                      })
                      setScanModels(p?.scan ? [] : scanModels)
                    }}
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm"
                  >
                    {tagPresets.map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.label}
                        {p.id === 'damai' ? t(' — 默认') : ''}
                      </option>
                    ))}
                  </select>
                </label>

                <label className="col-span-2 block">
                  <span className="text-xs text-ink-dim">{t('base_url（dam-ai / OpenAI 兼容，含 /v1）')}</span>
                  <input
                    value={form.tag.base_url}
                    onChange={(e) =>
                      setForm({ ...form, tag: { ...form.tag, base_url: e.target.value } })
                    }
                    placeholder={t('http://host.docker.internal:8091/v1（dam-ai tagger）')}
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>

                <label className="block">
                  <span className="text-xs text-ink-dim">
                    {t('模型名')}
                    {tagPresets.find((p) => p.id === form.tag.provider)?.scan && (
                      <Button
                        type="button"
                        onClick={() =>
                          void (async () => {
                            setBusy('scan')
                            try {
                              const r = await api.tagScan(form.tag.base_url)
                              setScanModels(r.models)
                              if (r.models.length > 0) {
                                setForm({
                                  ...form,
                                  tag: { ...form.tag, model: r.models[0] },
                                })
                              }
                              toast('success', t('扫描完成'), t('发现 {n} 个模型', { n: r.models.length }))
                            } catch (e) {
                              toast('error', t('扫描失败'), e instanceof Error ? e.message : String(e))
                            } finally {
                              setBusy(null)
                            }
                          })()
                        }
                        variant="link"
                        className="ml-2"
                      >
                        {t('扫描 Ollama 模型')}
                      </Button>
                    )}
                  </span>
                  {(() => {
                    const preset = tagPresets.find((p) => p.id === form.tag.provider)
                    const candidates = preset?.scan ? scanModels : (preset?.models ?? [])
                    if (candidates.length === 0) {
                      return (
                        <input
                          value={form.tag.model}
                          onChange={(e) =>
                            setForm({ ...form, tag: { ...form.tag, model: e.target.value } })
                          }
                          placeholder="qwen3.5-4b"
                          className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                        />
                      )
                    }
                    return (
                      <select
                        value={form.tag.model}
                        onChange={(e) =>
                          setForm({ ...form, tag: { ...form.tag, model: e.target.value } })
                        }
                        className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm"
                      >
                        {candidates.map((m) => (
                          <option key={m} value={m}>
                            {m}
                          </option>
                        ))}
                      </select>
                    )
                  })()}
                </label>

                <label className="block">
                  <span className="text-xs text-ink-dim">
                    {t('api_key（自建端点可留空）')}
                    {form.tag.provider === 'deepseek' && (
                      <span className="ml-1 text-accent">
                        {t('DeepSeek Key 在 platform.deepseek.com → API Keys 创建')}
                      </span>
                    )}
                  </span>
                  <input
                    value={form.tag.api_key}
                    onChange={(e) =>
                      setForm({ ...form, tag: { ...form.tag, api_key: e.target.value } })
                    }
                    placeholder={t('粘贴 API Key（sk-…）')}
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>

                <label className="block">
                  <span className="text-xs text-ink-dim">
                    max_tokens
                    <span className="ml-1 text-accent">
                      {t('当前生效')}{' '}
                      {form.tag.max_tokens === 0 ||
                      form.tag.max_tokens === DEFAULT_TAG_MAX_TOKENS
                        ? t('{n}（默认）', { n: DEFAULT_TAG_MAX_TOKENS })
                        : t('{n}（设置页覆盖）', { n: form.tag.max_tokens })}
                    </span>
                  </span>
                  <input
                    type="number"
                    min={0}
                    max={65536}
                    step={512}
                    value={form.tag.max_tokens}
                    onChange={(e) =>
                      setForm({
                        ...form,
                        tag: { ...form.tag, max_tokens: Number(e.target.value) },
                      })
                    }
                    data-testid="tag-max-tokens"
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                  {/* 直接键入覆盖 + 一键常用档位；上下箭头 step=512，不必从 2048 一档一档点。
                      KI-8b 棘轮：动作一律走 <Button> 载体，不写裸 button（灰字/淡边不算可点信号）。 */}
                  <div className="mt-1 flex flex-wrap items-center gap-1">
                    <span className="text-[11px] text-ink-dim">{t('快捷')}</span>
                    {[0, 2048, 4096, 8192, 16384].map((v) => (
                      <Button
                        key={v}
                        type="button"
                        data-testid={`tag-max-tokens-preset-${v}`}
                        variant="secondary"
                        size="sm"
                        active={form.tag.max_tokens === v}
                        onClick={() =>
                          setForm({ ...form, tag: { ...form.tag, max_tokens: v } })
                        }
                        className="text-[11px]"
                      >
                        {v === 0 ? t('默认 {n}', { n: DEFAULT_TAG_MAX_TOKENS }) : v}
                      </Button>
                    ))}
                  </div>
                  <span className="mt-1 block text-[11px] leading-snug text-ink-dim">
                    {t(
                      '可直接键入数字覆盖（不必用箭头）；箭头每档 512。默认 {default}：要覆盖「思考 token + 中文描述 + 10 个标签」的完整 JSON。填 0 = 用默认值；改小了会被截断（解析层能补齐结构，但标签会变少）。',
                      { default: DEFAULT_TAG_MAX_TOKENS },
                    )}
                  </span>
                </label>

                <label className="block">
                  <span className="text-xs text-ink-dim">{t('超时（毫秒）')}</span>
                  <input
                    type="number"
                    value={form.tag.timeout_ms}
                    onChange={(e) =>
                      setForm({
                        ...form,
                        tag: { ...form.tag, timeout_ms: Number(e.target.value) },
                      })
                    }
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>

                <label className="col-span-2 block">
                  <span className="text-xs text-ink-dim">{t('代理（留空 = 直连；云 API 按需填）')}</span>
                  <input
                    value={form.tag.proxy ?? ''}
                    onChange={(e) =>
                      setForm({ ...form, tag: { ...form.tag, proxy: e.target.value } })
                    }
                    placeholder={t('留空表示直连')}
                    className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1.5 text-sm outline-none focus:border-accent"
                  />
                </label>

                <label className="col-span-2 block">
                  <span className="text-xs text-ink-dim">
                    {t('三合一提示词模板（title_ch / title_en / description / tags JSON 约定）')}
                  </span>
                  <textarea
                    value={form.tag.prompt_template}
                    onChange={(e) =>
                      setForm({ ...form, tag: { ...form.tag, prompt_template: e.target.value } })
                    }
                    className="mt-1 min-h-28 w-full rounded border border-line bg-panel-2 px-2 py-1.5 font-mono text-xs outline-none focus:border-accent"
                  />
                </label>
              </div>

              <div className="flex items-center gap-2">
                <Button
                  type="button"
                  onClick={() => void save()}
                  disabled={busy !== null}
                  variant="secondary"
                >
                  {busy === 'save' ? t('保存中…') : t('保存')}
                </Button>
                <Button
                  type="button"
                  onClick={() =>
                    void (async () => {
                      setBusy('tagTest')
                      setTagTestResult(null)
                      try {
                        const r = await api.tagTest(form.tag)
                        if (r.ok) {
                          // 只留最终回复的首个非空行作「关键 token」，超长截断
                          const first = (r.reply ?? '')
                            .split('\n')
                            .map((s) => s.trim())
                            .filter(Boolean)[0]
                          const key = first ? (first.length > 48 ? `${first.slice(0, 48)}…` : first) : ''
                          setTagTestResult(
                            t('连接成功：{model} · {ms}ms{key}', {
                              model: r.model,
                              ms: r.latencyMs,
                              key: key ? t(' · {key}', { key }) : '',
                            }),
                          )
                          toast('success', t('打标端点可用'), r.model)
                        } else {
                          setTagTestResult(t('连接失败：{err}', { err: r.error ?? t('未知错误') }))
                          toast('error', t('打标端点不可用'), r.error ?? '')
                        }
                      } catch (e) {
                        const msg = e instanceof Error ? e.message : String(e)
                        setTagTestResult(t('连接失败：{err}', { err: msg }))
                        toast('error', t('打标端点不可用'), msg)
                      } finally {
                        setBusy(null)
                      }
                    })()
                  }
                  disabled={busy !== null}
                  variant="secondary"
                >
                  {busy === 'tagTest' ? t('测试中…') : t('测试连接')}
                </Button>
                {tagTestResult && (
                  <span
                    className={`text-xs ${tagTestResult.startsWith(t('连接成功')) ? 'text-emerald-400' : 'text-red-400'}`}
                  >
                    {tagTestResult}
                  </span>
                )}
              </div>
            </div>
          )}
        </section>

        {/* ---------- 向量索引（只留任务动作；只读诊断已收进底部「诊断」折叠区） ---------- */}
        <section className="rounded-lg border border-line bg-panel p-4" data-testid="settings-vector-actions">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-semibold">{t('向量索引（嵌入任务）')}</h2>
            <Button
              type="button"
              onClick={refresh}
              variant="secondary"
            >
              {t('刷新')}
            </Button>
          </div>
          <p className="mt-1 text-xs leading-5 text-ink-dim">
            {t('导入新图 / 换 provider 后在这里补向量。只读体检（向量表、行数、当前模型、库大小、版本）在页面底部「诊断」里按需展开。')}
          </p>

          <div className="mt-3 flex flex-wrap items-center gap-2">
            <Button
              type="button"
              disabled={running}
              onClick={() => void startIndex('pending')}
              variant="secondary"
            >
              {t('增量嵌入')}
            </Button>
            <Button
              type="button"
              disabled={running}
              onClick={() => {
                if (window.confirm(t('全库重嵌入：清空当前模型的队列并重算全部图片（旧向量表按模型保留）？'))) {
                  void startIndex('all')
                }
              }}
              variant="secondary"
            >
              {t('全库重嵌入')}
            </Button>
            <Button
              type="button"
              disabled={!running}
              onClick={() => void api.embedCancel().catch(() => {})}
              variant="secondary"
              tone="danger"
            >
              {t('取消')}
            </Button>
            <Button
              type="button"
              disabled={failures.length === 0 || running}
              onClick={() => void retryFailed()}
              variant="secondary"
            >
              {t('重置失败项（{n}）', { n: failures.length })}
            </Button>
            <Button
              type="button"
              onClick={() => navigate('/search')}
              variant="secondary"
              className="ml-auto"
            >
              {t('去搜索 →')}
            </Button>
          </div>

          {running && embed?.progress && (
            <p className="mt-2 text-xs text-ink-dim">
              {t('嵌入进行中：{index} / {total}（成功 {done}，失败 {failed}）', {
                index: embed.progress.index,
                total: embed.progress.total,
                done: embed.progress.doneCount,
                failed: embed.progress.failedCount,
              })}
            </p>
          )}
          {!running && embed?.lastError && (
            <p className="mt-2 text-xs text-red-400">{t('上次任务错误：{err}', { err: embed.lastError })}</p>
          )}

          {failures.length > 0 && (
            <div className="mt-4 rounded border border-red-600/40 bg-red-950/20 p-2">
              <p className="text-xs font-semibold text-red-300">
                {t('失败明细（最近 {n} 条）', { n: failures.length })}
              </p>
              <ul className="mt-1 space-y-0.5">
                {failures.slice(0, 8).map((f) => (
                  <li key={f.jobId} className="truncate text-[11px] text-red-200/90">
                    {t('asset {id} · 尝试 {n} · {err}', { id: f.assetId, n: f.attempts, err: f.lastError })}
                  </li>
                ))}
              </ul>
              <p className="mt-1 text-[11px] text-ink-dim">
                {t('常见原因：端点不可达（设置页「测试连接」）/ 单图过大 / 文件已失联。')}
              </p>
            </div>
          )}
        </section>

        {/* KI-10：库根列表（每个根都可「打开文件夹」） */}
        <LibraryRoots />

        <section className="rounded-lg border border-line bg-panel p-4">
          <h2 className="text-sm font-semibold">{t('关于 provider 切换')}</h2>
          <ul className="mt-2 list-inside list-disc space-y-1 text-xs leading-5 text-ink-dim">
            <li>{t('dam-ai 三引擎（embedder /v1/embeddings）是默认自托管选项，不是唯一选项。')}</li>
            <li>{t('任何 OpenAI 兼容 embedding 端点（dashscope、JinaCLIP、自建 vLLM）都能填在这里。')}</li>
            <li>{t('换端点模型 = 换向量表：旧向量不会被静默混算，重新嵌入即可。')}</li>
            <li>{t('桌面版（Tauri）额外支持本地 ONNX 模型（CPU 离线），web 版不背 ONNX 运行时。')}</li>
          </ul>
        </section>

        <section className="rounded-lg border border-line bg-panel p-4" data-testid="exif-settings">
          <h2 className="text-sm font-semibold">{t('拍摄元数据（EXIF）')}</h2>
          <p className="mt-1 text-xs leading-5 text-ink-dim">
            {t(
              '导入时解析拍摄时间 / 相机 / 镜头 / ISO / 焦距等，并写入可检索列（拍摄时间排序、相机筛选、横竖方图）。GPS 属隐私数据，默认完全关闭：开启前不会读取、不会存储任何位置信息。',
            )}
          </p>

          <label className="mt-3 flex items-center gap-2 text-sm" data-testid="exif-parse-toggle">
            <input
              type="checkbox"
              checked={exifCfg?.parseOnImport ?? true}
              onChange={(e) =>
                setExifCfg((c) => (c ? { ...c, parseOnImport: e.target.checked } : c))
              }
            />
            {t('导入时解析 EXIF（推荐开启）')}
          </label>

          <div className="mt-3 flex flex-wrap items-center gap-2 text-sm" data-testid="exif-gps-mode">
            <span className="text-ink-dim">{t('GPS 档位')}</span>
            {(['off', 'coords', 'place'] as const).map((m) => (
              <Button
                key={m}
                type="button"
                data-testid={`exif-gps-${m}`}
                onClick={() => setExifCfg((c) => (c ? { ...c, gps: m } : c))}
                variant="secondary"
                size="none"
                active={(exifCfg?.gps ?? 'off') === m}
                className="px-2 py-0.5 text-xs"
              >
                {m === 'off' ? t('关闭（默认）') : m === 'coords' ? t('存坐标') : t('只存地名')}
              </Button>
            ))}
          </div>

          {exifCfg?.gps === 'place' && (
            <div className="mt-3 grid grid-cols-1 gap-2 text-xs sm:grid-cols-3" data-testid="exif-gps-provider">
              <input
                placeholder={t('provider（nominatim）')}
                value={exifCfg.gpsProvider.name}
                onChange={(e) =>
                  setExifCfg((c) =>
                    c ? { ...c, gpsProvider: { ...c.gpsProvider, name: e.target.value } } : c,
                  )
                }
                className="rounded border border-line bg-panel-2 px-2 py-1 outline-none focus:border-accent"
              />
              <input
                placeholder={t('API key（用户自有，可空）')}
                value={exifCfg.gpsProvider.apiKey}
                onChange={(e) =>
                  setExifCfg((c) =>
                    c ? { ...c, gpsProvider: { ...c.gpsProvider, apiKey: e.target.value } } : c,
                  )
                }
                className="rounded border border-line bg-panel-2 px-2 py-1 outline-none focus:border-accent"
              />
              <input
                placeholder={t('base_url（自建端点，可空）')}
                value={exifCfg.gpsProvider.baseUrl}
                onChange={(e) =>
                  setExifCfg((c) =>
                    c ? { ...c, gpsProvider: { ...c.gpsProvider, baseUrl: e.target.value } } : c,
                  )
                }
                className="rounded border border-line bg-panel-2 px-2 py-1 outline-none focus:border-accent"
              />
            </div>
          )}

          <p className="mt-2 rounded border border-amber-600/40 bg-amber-950/20 px-2 py-1.5 text-xs leading-5 text-amber-300/90" data-testid="exif-privacy-note">
            {t(
              '隐私提示：开启「存坐标」后，GPS 经纬度会存入本地图库并随导出清单带出（导出可选剥离坐标）；「只存地名」不落任何坐标，仅保留城市/区域文本。GPS 关闭时连读取都不会发生。',
            )}
          </p>

          <div className="mt-3 flex flex-wrap items-center gap-2">
            <Button
              type="button"
              data-testid="exif-save"
              disabled={busy === 'exif-save'}
              onClick={() => {
                if (!exifCfg) return
                setBusy('exif-save')
                api
                  .exifSettingsSave(exifCfg)
                  .then((c) => {
                    setExifCfg(c)
                    toast('success', t('已保存'), t('GPS 档位：{mode}', { mode: c.gps }))
                    return api.exifStatus().then(setExifStatus)
                  })
                  .catch((e) => toast('error', t('保存失败'), e instanceof Error ? e.message : String(e)))
                  .finally(() => setBusy(null))
              }}
              variant="secondary"
            >
              {t('保存')}
            </Button>
            <Button
              type="button"
              data-testid="exif-backfill"
              disabled={busy === 'exif-backfill'}
              onClick={() => {
                setBusy('exif-backfill')
                api
                  .exifBackfill({})
                  .then((r) => {
                    toast(
                      'success',
                      t('存量回填完成'),
                      t('本轮处理 {processed}（完成 {done} / 失败 {failed}），入队 {enqueued}', {
                        processed: r.processed,
                        done: r.done,
                        failed: r.failed,
                        enqueued: r.enqueued,
                      }),
                    )
                    return api.exifStatus().then(setExifStatus)
                  })
                  .catch((e) => toast('error', t('回填失败'), e instanceof Error ? e.message : String(e)))
                  .finally(() => setBusy(null))
              }}
              variant="secondary"
            >
              {t('存量回填（未解析图片）')}
            </Button>
            {exifStatus && (
              <span className="text-xs text-ink-dim" data-testid="exif-status-line">
                {t('待解析 {n} · 队列 done {done} / failed {failed}{last}', {
                  n: exifStatus.remaining,
                  done: exifStatus.jobDone,
                  failed: exifStatus.jobFailed,
                  last: exifStatus.lastError ? t(' · 最近失败：{err}', { err: exifStatus.lastError }) : '',
                })}
              </span>
            )}
          </div>
        </section>

        {/* ---------- 文字识别（OCR）：卡 task 从打标任务页整块迁入（一能力一处） ---------- */}
        <OcrSettings landed={landed === 'ocr'} />

        {/* ---------- 模型文件（可选下载）：卡 task —— lama-fp32 之前没有任何下载入口 ---------- */}
        <ModelFilesSettings />

        {/* ---------- 处理插件 + 方案：入口在设置，载体是 Drawer（`?drawer=packs`） ----------
            UI-CARRIER-1 归属单一：设置页只留一句入口说明 + 一个按钮，实体（安装/版本/回滚/卸载/
            详情、方案的建/改/删）落在右侧 Drawer。判定的执行通道恒为处理插件 —— 用户只选方案，
            不存在「切引擎」这种步骤。 */}
        <section
          id="specs"
          className="rounded-lg border border-line bg-panel p-4"
          data-testid="packs-specs-entry"
        >
          <h2 className="text-sm font-semibold">{t('处理插件与方案')}</h2>
          <p className="mt-1 text-xs leading-5 text-ink-dim">
            {t('能做什么 = 插件（规则/算法 + 默认参数，规则、词表、报告元数据都在')}{' '}
            <code>.dampack</code>
            {t('包内，装/卸/换版本在抽屉里）；做成什么样 = 方案（选规则 + 阈值 + 动作链 + 输出，用户自建、不安装）。判定与处理只选方案，运行结果在任务中心。')}
          </p>
          <Button
            type="button"
            onClick={() => openOverlay('packs')}
            data-testid="open-packs-drawer"
            variant="secondary"
          >
            {t('管理处理插件与方案')}
          </Button>
        </section>

        {/* ---------- 诊断（折叠，默认收起）：向量表/行数/当前模型/库大小/版本 ---------- */}
        <Diagnostics />
      </div>
    </div>
  )
}

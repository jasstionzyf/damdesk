/**
 * 能力详情页（N4 起，KI-35 P2 卡 task 对齐 2026-10-24 用户面命名表）——一个能力只有这一面。
 *
 * 口径：`damdesk-product/references/plugin-design-and-ui.md` §3/§3.1/§4 + `plugin-param-schema.md` §9。
 * 三段结构（顺序不许换，用户按这个顺序读）：
 *
 * | 区 | 内容 | 能否改 |
 * |---|---|---|
 * | 能力说明 | 干什么 / 要什么图 / 给我什么 / 引擎与依赖 / 版本与体积 | 只读 |
 * | **我的方案** | 该能力下的方案列表（淘宝主图 / 亚马逊主图 / …），编辑用同一份 `ParamForm` | 可增删改（改的是方案，不动默认设置） |
 * | 默认设置 | 能力自带 schema 渲染的控件 | 只读 |
 *
 * 文案红线（§3 用户面命名表，2026-10-24 裁决）：用户可见文案不出现 插件/规格/manifest/任务/job/verdict ——
 * 统一叫 方案 / 默认设置 / 处理记录 / 通过·未通过·待人工看（例外：本页底部「安装与版本」是包管理面，
 * 装/卸/换版本照 KI-35 口径允许出现「插件/包」）。
 *
 * 硬边界（违反即错）：
 * 1. **不放批量进度 / 历史结果 / 失败列表** —— 那些归处理记录（一个能力只归一个区域）；
 *    这里只放「结果在哪看」的链接。**先试一张是唯一例外**：单张的即时结果就地展示
 *    （判定类给 通过/未通过/待人工看 + 原因，处理类给产物缩略图），它是「参数错了当场看见」的试跑，不是结果面。
 * 2. 未安装：schema 照样可见，控件灰态 + 「装好后可用」+ 去设置装包的入口；方案照样可见可编辑（卸载不删用户配置）。
 * 3. 检查类才有「复核」字样；处理类不提审核列（`kinds` 由包声明，界面不猜）。
 * 4. 安装/切版本/回滚/卸载与装载态**只在这里**（列表页只留导航与首次安装），不留两套页。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import {
  api,
  type ComplianceInstalledPluginPack,
  type CompliancePluginPackDetail,
  type ComplianceResultRowView,
  type ComplianceSpecView,
  type ComplianceSignaturePolicyView,
  type ParamSchemaDoc,
  type PluginParamsSchemaView,
} from '../lib/api'
import { useConfirm, verbCount } from '../lib/confirm'
import { useToast } from '../lib/toast'
import { Button } from '../components/Button'
import { ParamForm } from '../components/ParamForm'
import { PackInstaller } from '../components/PackInstaller'
import { SpecEditorDialog } from '../components/SpecEditorDialog'
import { pickI18nOr, usePackTextLocale, type PackText } from '../i18n/pickI18n'
import { kindWord, resolvePlugin, runtimeOf, sigClass, sigLabel } from '../lib/plugins'
import { pickDefaultSpecId, setDefaultSpecId } from '../lib/defaultSpec'
import { notifyPacksChanged } from '../lib/capabilityNav'
import { RESULT_LABELS, VERDICT_LABELS, type PluginKind } from '../lib/pluginResult'
import { useI18n } from '../i18n'

/** 结论三态的徽标配色（通过 / 未通过 / 待人工看） */
const VERDICT_CLS: Record<string, string> = {
  pass: 'border-emerald-600/60 bg-emerald-600/10 text-emerald-300',
  fail: 'border-rose-600/60 bg-rose-600/10 text-rose-300',
  undetermined: 'border-amber-600/60 bg-amber-600/10 text-amber-300',
}

/** 「先试一张」的就地结果（单张试跑；批量的进度/历史不在此页，见硬边界 1） */
interface TryRunState {
  specName: string
  runId: number
  phase: 'running' | 'done' | 'error'
  rows: ComplianceResultRowView[]
  kinds: PluginKind[]
  err: string | null
}

/** 能力说明的语义行（内容全部由 manifest / schema / 方案**推导**，不写死业务话术） */
interface CapabilityRow {
  key: string
  label: string
  text: string
  hint?: string
}

export function PluginDetail() {
  const packLocale = usePackTextLocale()
  const { pluginId = '' } = useParams()
  const { toast } = useToast()
  const confirm = useConfirm()
  const { t } = useI18n()

  // IP-K16 之后插件事实只有一个来源（账本 versions），见 lib/plugins.resolvePlugin。
  const [packs, setPacks] = useState<{
    plugins?: ComplianceInstalledPluginPack[]
  } | null>(null)
  const [detail, setDetail] = useState<CompliancePluginPackDetail | null>(null)
  const [policy, setPolicy] = useState<ComplianceSignaturePolicyView | null>(null)
  const [hostCaps, setHostCaps] = useState<string[]>([])
  const [schemaView, setSchemaView] = useState<PluginParamsSchemaView | null>(null)
  const [schemaErr, setSchemaErr] = useState<string | null>(null)
  const [specs, setSpecs] = useState<ComplianceSpecView[]>([])
  const [defaultSpecId, setDefaultId] = useState<number | null>(null)
  const [editor, setEditor] = useState<{ spec: ComplianceSpecView | null } | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [err, setErr] = useState<string | null>(null)
  /** 「先试一张」的就地结果（KI-35 P2：不再跳走，单张结果原地显示） */
  const [tryRun, setTryRun] = useState<TryRunState | null>(null)
  const tryAlive = useRef(true)
  useEffect(() => {
    tryAlive.current = true
    return () => {
      tryAlive.current = false
    }
  }, [])

  const refresh = useCallback(async () => {
    if (!pluginId) return
    setErr(null)
    try {
      const res = await api.compliancePacks()
      setPacks({
        plugins: res.plugins,
      })
      setPolicy(res.signaturePolicy ?? null)
      setHostCaps(res.hostCapabilities ?? [])
      // 包详情（manifest + 规则清单）：未安装/坏包也给结构（error 里带原因）
      try {
        setDetail(await api.compliancePluginPackDetail(pluginId, false))
      } catch (e) {
        setDetail(null)
        setErr(e instanceof Error ? e.message : String(e))
      }
      // 参数声明：404 = 包不在（-> 灰态），500 = 包被改坏（要看见原文）
      try {
        setSchemaView(await api.pluginParamsSchema(pluginId))
        setSchemaErr(null)
      } catch (e) {
        setSchemaView(null)
        setSchemaErr(e instanceof Error ? e.message : String(e))
      }
      const list = await api.specs(pluginId)
      setSpecs(list.items)
      setDefaultId(pickDefaultSpecId(pluginId, list.items))
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
    }
  }, [pluginId])

  useEffect(() => {
    void refresh()
  }, [refresh])

  const rp = useMemo(() => resolvePlugin(packs ?? {}, pluginId), [packs, pluginId])
  /** 账本行 = 有多版本栈（可切/可回滚/可卸）；只有内置身份时这些动作不适用 */
  const hasLedger = (packs?.plugins ?? []).some((p) => p.id === pluginId)
  const installed = rp.runtime?.available === true
  const kinds = useMemo<PluginKind[] | null>(() => {
    const raw = detail?.manifest?.kinds
    if (Array.isArray(raw)) return raw.filter((k): k is PluginKind => k === 'judge' || k === 'process')
    // manifest 在但没声明 kinds → 协议回落判定类（历史包装的都是判定类）；manifest 不在 → 不知道
    return detail?.manifest ? ['judge'] : null
  }, [detail])

  const rules = useMemo(() => {
    const fromPack = detail?.rules?.map((r) => r.id) ?? []
    if (fromPack.length > 0) return fromPack
    return specs[0]?.pluginStatus.rulesAvailable ?? []
  }, [detail, specs])

  /**
   * KI-31 之后判定按**规格自己的 pluginId** 开包（宿主只开那一个包），所以「当前判定引擎跑的是
   * 别的插件」不再是失败前置条件 —— 这里不再因为引擎身份把「先跑 1 张」灰掉。
   */
  const schema: ParamSchemaDoc | null = schemaView?.declared ? schemaView.schema : null
  // i18n-F：包自带名（short_name 优先 → name），对象形态按当前语言取（切语言即时切换）
  const pluginName =
    pickI18nOr(
      (detail?.manifest?.short_name ?? detail?.manifest?.name) as PackText,
      (detail?.manifest?.short_name as string | undefined) ??
        (detail?.manifest?.name as string | undefined) ??
        null,
      packLocale,
    ).trim() ||
    rp.name ||
    pluginId

  const capability = useMemo<CapabilityRow[]>(() => {
    // 读不到包的 kinds 声明（包详情取失败）时按检查类看待 —— 历史包装的都是判定类，
    // 但必须**说出来**（下面 what 行前缀），不能默默当成结论
    const ks = kinds ?? ['judge']
    const judge = ks.includes('judge')
    const process = ks.includes('process')
    const what = judge
      ? rules.length > 0
        ? t('按 {n} 条标准逐张给结论（通过 / 未通过 / 待人工看）', { n: rules.length })
        : t('按官方标准逐张给结论（通过 / 未通过 / 待人工看）')
      : t('把图处理成你要的样子（处理后的图存为这张图的新版本，原图不动）')
    const needs = [
      t('一张能打开的原图（jpg / png / webp 等常见格式）'),
      rules.length > 0
        ? t('按这些标准来：{list}', { list: rules.slice(0, 4).join(' / ') }) +
          (rules.length > 4 ? t(' …（共 {n} 条）', { n: rules.length }) : '')
        : t('标准清单以官方声明为准（安装后可见）'),
    ]
    const gives = [
      judge ? t('每张图 × 每条标准的结论 + 原因 + 把握度（哪张能用、哪张要改、哪张机器拿不准）') : '',
      process ? t('处理后的图片：自动存为该图的新版本（原图永远不动），也可导出成文件') : '',
    ].filter(Boolean)
    const caps = (detail?.manifest?.capabilities as string[] | undefined) ?? []
    const engine = [
      t('入口 {cmd}', { cmd: ((detail?.manifest?.entrypoint as { cmd?: string } | undefined)?.cmd) ?? '—' }),
      detail?.manifest?.deterministic === true
        ? t('确定性执行（同图同结果）')
        : detail?.manifest?.deterministic === false
          ? t('含模型/VL（同图结果可能不完全一致）')
          : t('确定性未知'),
      t('超时 {n}s', { n: String(detail?.manifest?.timeout_secs ?? '—') }),
      caps.length > 0 ? t('依赖能力 {caps}', { caps: caps.join(' / ') }) : t('能力未声明'),
      detail?.manifest?.permissions
        ? t('权限 读盘 {read} · 联网 {net}', {
            read: (detail.manifest.permissions as { read?: boolean }).read ? t('是') : t('否'),
            net: (detail.manifest.permissions as { net?: boolean }).net ? t('是') : t('否'),
          })
        : '',
    ].filter(Boolean)
    return [
      {
        key: 'what',
        label: t('干什么'),
        text:
          t('{name} —— {kind}类能力：{what}', {
            name: pluginName,
            kind: kindWord(kinds),
            what,
          }) + (kinds === null ? t('（没读到能力类型声明，暂按检查类看待）') : ''),
      },
      { key: 'needs', label: t('要什么图'), text: needs.join(t('；')) },
      { key: 'gives', label: t('给我什么'), text: gives.join(t('；')) || t('（能力类型未声明，按检查类看待）') },
      { key: 'engine', label: t('引擎与依赖'), text: engine.join(' · ') },
      {
        key: 'version',
        label: t('版本与体积'),
        // 体积宿主还没上报（`/api/compliance/packs` 没这个字段）—— 就写「未上报」，不拿 0 冒充
        text: t('当前版本 {v} · 体积 未上报', {
          v: rp.currentVersion ?? detail?.currentVersion ?? detail?.version ?? '—',
        }),
        hint: t('目录：{dir}（装包回执里有归档文件名与落盘位置）', { dir: detail?.dir ?? '—' }),
      },
    ]
  }, [detail, kinds, pluginName, rp.currentVersion, rules, t])

  const switchVersion = async (version: string | undefined, mode: 'activate' | 'rollback') => {
    setBusy(`${mode}:${version ?? ''}`)
    try {
      const r =
        mode === 'activate'
          ? await api.compliancePackActivateVersion(pluginId, version)
          : await api.compliancePackRollback(pluginId, version)
      toast(
        'success',
        mode === 'activate' ? t('已切换当前版本') : t('已回滚'),
        t('{id}：{from} → {to}（旧版本目录保留，可再切回）', {
          id: r.id,
          from: r.from ?? t('（无）'),
          to: r.to,
        }),
      )
      await refresh()
      // KI-35 P1：版本切换影响「当前版本在不在」→ 导航能力区跟着增减，立即广播
      notifyPacksChanged()
    } catch (e) {
      toast('error', mode === 'activate' ? t('切换版本失败') : t('回滚失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  const uninstall = async (version: string) => {
    // KI-35 P4 走查缺口闭环：只剩这一个已装版本时，按版本卸会被 409 pack_version_current
    // 拒掉（当前版本保护）→「最后一个版本」改走整包卸载（不带 version：目录全删、账本
    // 留审计、current 置空 → 主导航能力区立即减项）。还有别的版本在，才保留「先切走再卸」。
    const lastPresent = present.length === 1 && present[0].version === version
    const ok = await confirm({
      title: t('卸载 {name} {version}？', { name: pluginName, version }),
      body: lastPresent
        ? t('这是最后一个已装版本：整包卸载（包目录全删；历史结论与账本行都保留，不会丢；方案也不删）。')
        : t('卸载 = 删除该版本的包目录（历史结论与账本行都保留，不会丢；方案也不删）。'),
      impacts: [
        t('{name} {version}{extra} 的包目录会被删除', {
          name: pluginName,
          version,
          extra: lastPresent ? t('（最后一个版本，卸完该能力从导航消失）') : '',
        }),
      ],
      recoverable: t('不可撤销：要恢复只能重新安装该版本 .dampack'),
      confirmLabel: verbCount(t('卸载'), 1, t('个版本')),
      danger: true,
    })
    if (!ok) return
    setBusy(`uninstall:${version}`)
    try {
      const r = await api.compliancePackUninstall(pluginId, lastPresent ? undefined : version)
      toast(
        'success',
        t('已卸载该版本'),
        t('{id}：移除 {removed} · 剩余 {remaining}', {
          id: r.id,
          removed: r.removed.join(', ') || '—',
          remaining: r.remaining.join(', ') || t('（无）'),
        }) +
          (r.staleCurrent ? t(' · 当前指针失效（{id}），请重装或切版本', { id: r.staleCurrent }) : ''),
      )
      await refresh()
      // KI-35 P1：卸载 = 导航能力区立即减项，广播给主导航重拉注册表
      notifyPacksChanged()
    } catch (e) {
      toast('error', t('卸载失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  const removeSpec = async (spec: ComplianceSpecView) => {
    const ok = await confirm({
      title: t('删除方案「{name}」？', { name: spec.name }),
      body: t('只删这条配置：历史结论、处理后的图、能力包都不动（历史结果仍按当时用的参数可回看）。'),
      impacts: [t('方案「{name}」的标准/参数设置会消失', { name: spec.name })],
      recoverable: t('不可撤销：要恢复只能重新建一条'),
      confirmLabel: t('删除方案'),
      danger: true,
    })
    if (!ok) return
    setBusy(`spec-delete:${spec.specId}`)
    try {
      await api.specDelete(spec.specId)
      if (defaultSpecId === spec.specId) setDefaultSpecId(pluginId, null)
      toast('success', t('方案已删除'), t('{name}（历史结论与处理后的图都保留）', { name: spec.name }))
      await refresh()
    } catch (e) {
      toast('error', t('删除失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  const makeDefault = (spec: ComplianceSpecView) => {
    setDefaultSpecId(pluginId, spec.specId)
    setDefaultId(spec.specId)
    toast('success', t('已设为默认方案'), t('{name}：检查/处理入口默认选它（这台机器上的偏好）', { name: spec.name }))
  }

  /**
   * 「先试一张」的收尾轮询：这一次跑完（或失败/被取消）后把单张结果拉回来就地显示。
   * 只轮询这一次 run —— 批量进度/历史归处理记录，这里不做进度条（硬边界 1）。
   */
  const checkTry = async (runId: number, depth: number) => {
    const patch = (p: Partial<TryRunState>) =>
      setTryRun((cur) => (cur && cur.runId === runId ? { ...cur, ...p } : cur))
    try {
      const job = await api.complianceJob(runId, { limit: 1 })
      if (job.run.status === 'running' && depth < 150) {
        if (tryAlive.current) window.setTimeout(() => void checkTry(runId, depth + 1), 900)
        return
      }
      if (job.run.status === 'running') {
        // 轮询到顶还在跑（约 2 分钟）：不无限等，给出路
        patch({ phase: 'error', err: t('这一次跑得太久还没出结果 —— 去「处理记录」看进度，跑完随时回来看。') })
        return
      }
      const view = await api.complianceResults({ runId, limit: 50 })
      if (!tryAlive.current) return
      patch({
        phase: 'done',
        rows: view.items,
        kinds: (view.kinds as PluginKind[] | undefined) ?? [],
      })
    } catch (e) {
      if (tryAlive.current) patch({ phase: 'error', err: e instanceof Error ? e.message : String(e) })
    }
  }

  const tryOne = async () => {
    if (defaultSpecId == null) {
      toast('warning', t('先建一条方案'), t('还没有自己的方案（或都停用了）：点「新建方案」存一套参数，再回来试'))
      return
    }
    const spec = specs.find((s) => s.specId === defaultSpecId)
    setBusy('try')
    try {
      const r = await api.specTry(defaultSpecId)
      const specName = spec?.name ?? t('方案 #{id}', { id: defaultSpecId })
      setTryRun({
        specName,
        runId: r.runId,
        phase: 'running',
        rows: [],
        kinds: kinds ?? [],
        err: null,
      })
      void checkTry(r.runId, 0)
      toast('success', t('先试一张跑起来了'), t('「{name}」· 入队 {n} 张，结果马上显示在这里', { name: specName, n: r.queuedAssets }))
    } catch (e) {
      toast('error', t('先试一张没跑起来'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(null)
    }
  }

  if (!pluginId) {
    return <p className="p-4 text-xs text-ink-dim">{t('没指定能力 id。')}</p>
  }

  const rt = rp.runtime ? runtimeOf(rp.runtime) : null
  const versions = rp.versions
  const present = versions.filter((v) => v.present)

  return (
    <div
      className="mx-auto w-full max-w-4xl flex-1 overflow-y-auto px-4 py-4"
      data-testid="plugin-detail"
    >
      {/* ------------------------------------------------ 页头：能力身份 + 两个固定动作 */}
      <header className="flex flex-wrap items-start gap-2">
        <div className="min-w-0">
          <h1 className="truncate text-sm font-semibold text-ink" data-testid="plugin-detail-title">
            {pluginName}
          </h1>
          <p className="mt-1 flex flex-wrap items-center gap-1.5 text-[11px] text-ink-dim">
            <code className="rounded border border-line bg-panel-2 px-1" data-testid="plugin-detail-id">
              {pluginId}
            </code>
            <span
              data-testid="plugin-detail-builtin"
              className={`rounded border px-1.5 py-0.5 text-[10px] ${
                rp.builtin ? 'border-accent/60 bg-accent/10 text-accent-soft' : 'border-line text-ink-dim'
              }`}
              title={rp.builtin ? t('随产品分发的内置包（可升级/切版本，不可卸载）') : t('第三方安装包')}
            >
              {rp.builtin ? t('内置') : t('第三方')}
            </span>
            {kinds && (
              <span className="rounded border border-line px-1.5 py-0.5 text-[10px]" data-testid="plugin-detail-kinds">
                {kindWord(kinds)}
              </span>
            )}
            {rt && (
              <span
                data-testid="plugin-detail-runtime"
                title={rt.hint}
                className={`rounded border px-1.5 py-0.5 text-[10px] ${rt.cls}`}
              >
                {t('装载态 {word}', { word: rt.word })}
              </span>
            )}
          </p>
        </div>
        <span className="flex-1" />
        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="secondary"
            data-testid="plugin-spec-new"
            onClick={() => setEditor({ spec: null })}
          >
            {t('新建方案')}
          </Button>
          <Button
            type="button"
            data-testid="plugin-try-one"
            disabled={busy !== null || defaultSpecId == null}
            title={
              defaultSpecId == null
                ? t('还没有可用方案：先新建一条（方案 = 参数 + 规则 + 输出）')
                : t('拿一张图先试试效果，满意再整批跑（用默认方案）')
            }
            onClick={() => void tryOne()}
          >
            {busy === 'try' ? t('试一张中…') : t('先试一张')}
          </Button>
        </div>
      </header>

      {err && (
        <p className="mt-2 rounded border border-rose-600/60 bg-rose-600/10 px-2 py-1 text-xs text-rose-300" data-testid="plugin-detail-error">
          {err}
        </p>
      )}

      {/* ---------------- 「先试一张」的就地结果（单张试跑；批量进度/历史不在此页） */}
      {tryRun && (
        <section className="mt-3 rounded-lg border border-line bg-panel p-3" data-testid="plugin-try-result">
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-xs font-semibold text-ink">{t('先试一张的结果')}</h2>
            <span className="text-[11px] text-ink-dim">
              {t('方案「{name}」 · 处理记录 #{id}', { name: tryRun.specName, id: tryRun.runId })}
            </span>
            <span className="flex-1" />
            <Link
              to={`/tasks?task=${tryRun.runId}`}
              className="text-[11px] text-accent hover:underline"
              data-testid="plugin-try-open-record"
            >
              {t('整批跑 / 看全部明细 →')}
            </Link>
          </div>
          {tryRun.phase === 'running' && (
            <p className="mt-2 text-[11px] text-ink-dim" data-testid="plugin-try-running">
              {t('正在试这一张，跑完自动显示结果（先去做别的也行）。')}
            </p>
          )}
          {tryRun.phase === 'error' && (
            <p
              className="mt-2 rounded border border-amber-600/60 bg-amber-600/10 px-2 py-1 text-[11px] text-amber-300"
              data-testid="plugin-try-error"
            >
              {tryRun.err}
            </p>
          )}
          {tryRun.phase === 'done' &&
            (tryRun.rows.length === 0 ? (
              <p className="mt-2 text-[11px] text-ink-dim" data-testid="plugin-try-empty">
                {t('这一次没有结果行（那张图可能被跳过：文件不在了 / 不是图片）。')}
              </p>
            ) : tryRun.kinds.includes('judge') ? (
              <ul className="mt-2 divide-y divide-line/60">
                {tryRun.rows.map((row, i) => (
                  <li
                    key={`${row.assetId}-${row.ruleId}-${i}`}
                    className="flex flex-wrap items-center gap-2 py-1.5 text-[11px]"
                    data-testid={`plugin-try-row-${i}`}
                  >
                    <span
                      className={`rounded border px-1.5 py-0.5 text-[10px] ${
                        row.verdict ? VERDICT_CLS[row.verdict.status] : 'border-line text-ink-dim'
                      }`}
                    >
                      {row.verdict
                        ? t(VERDICT_LABELS[row.verdict.status])
                        : t(RESULT_LABELS[row.result] ?? row.result)}
                    </span>
                    <code className="text-ink">{row.ruleId || t('整体')}</code>
                    <span className="min-w-0 flex-1 text-ink-dim">
                      {row.verdict?.reasonCode ?? row.reason ?? ''}
                    </span>
                    {row.verdict?.confidence != null && (
                      <span className="text-ink-dim">{t('把握度 {n}%', { n: Math.round(row.verdict.confidence * 100) })}</span>
                    )}
                  </li>
                ))}
              </ul>
            ) : (
              <div className="mt-2 flex items-start gap-3">
                {tryRun.rows[0] && (
                  <img
                    src={api.thumbUrl(tryRun.rows[0].assetId, 1024)}
                    alt={t('处理后的图')}
                    className="h-24 rounded border border-line object-contain"
                    data-testid="plugin-try-thumb"
                  />
                )}
                <div className="min-w-0 text-[11px]">
                  {tryRun.rows.map((row, i) => (
                    <p key={i} data-testid={`plugin-try-row-${i}`}>
                      {t(RESULT_LABELS[row.result] ?? row.result)}
                      {row.result === 'ok'
                        ? t('：处理后的图已存为这张图的新版本（原图不动）')
                        : row.reason
                          ? t('：{reason}', { reason: row.reason })
                          : ''}
                    </p>
                  ))}
                  <p className="mt-1 text-ink-dim">{t('满意就整批跑；不满意改方案参数再试一张。')}</p>
                </div>
              </div>
            ))}
        </section>
      )}

      {/* ------------------------------------------------ ① 能力说明（只读） */}
      <section className="mt-4 rounded-lg border border-line bg-panel p-3" data-testid="plugin-section-capability">
        <h2 className="text-xs font-semibold text-ink">{t('能力说明')}</h2>
        <dl className="mt-2 divide-y divide-line/60">
          {capability.map((row) => (
            <div key={row.key} className="flex gap-2 py-1.5 text-[11px]" data-testid={`plugin-cap-${row.key}`}>
              {/* 标签列不锁 80px：英文（Engine & dependencies）在 1024 档会折成 3 行 —— i18n-G 三档布局探针实测 */}
              <dt className="shrink-0 whitespace-nowrap text-ink-dim">{row.label}</dt>
              <dd className="min-w-0 flex-1 text-ink" title={row.hint}>
                {row.text}
              </dd>
            </div>
          ))}
        </dl>
        {kinds?.includes('judge') ? (
          <p className="mt-2 rounded border border-line bg-panel-2 px-2 py-1 text-[11px] text-ink-dim" data-testid="plugin-review-note">
            {t('检查类：每张图的结论与改判都在')}
            <Link to={`/tasks?pack=${encodeURIComponent(pluginId)}`} className="mx-1 text-accent hover:underline">
              {t('处理记录')}
            </Link>
            {t('里按次查看（可复核 / 看低把握度的图）。')}
          </p>
        ) : (
          <p className="mt-2 rounded border border-line bg-panel-2 px-2 py-1 text-[11px] text-ink-dim" data-testid="plugin-process-note">
            {t('处理类：没有通过与未通过的结论 —— 处理后的图直接存为该图的新版本；结果在处理记录里按图片看。')}
          </p>
        )}
      </section>

      {/* ------------------------------------------------ ② 我的方案（可增删改） */}
      <section className="mt-3 rounded-lg border border-line bg-panel p-3" data-testid="plugin-section-specs">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-xs font-semibold text-ink">{t('我的方案')}</h2>
          <span className="text-[11px] text-ink-dim">
            {t('一套检查或处理的标准和参数，保存后随时可复用（如 淘宝主图 / 公司官网 / 朋友圈压缩）')}
          </span>
        </div>

        {specs.length === 0 ? (
          <p className="mt-2 rounded border border-line bg-panel-2 px-2 py-1 text-[11px] text-ink-dim" data-testid="plugin-specs-empty">
            {t('还没有自己的方案。点「新建方案」，按你的平台标准存一套参数，以后一键复用。')}
          </p>
        ) : (
          <table className="mt-2 w-full text-[11px]">
            <thead className="text-ink-dim">
              <tr>
                <th className="py-0.5 text-left font-medium">{t('方案名')}</th>
                <th className="py-0.5 text-left font-medium">{t('规则')}</th>
                <th className="py-0.5 text-left font-medium">{t('参数改动')}</th>
                <th className="py-0.5 text-right font-medium">{t('操作')}</th>
              </tr>
            </thead>
            <tbody>
              {specs.map((s) => {
                const changed = Object.keys(s.paramsDiff ?? {}).length
                return (
                  <tr key={s.specId} className="border-t border-line" data-testid={`spec-row-${s.specId}`}>
                    <td className="py-1 pr-2 text-ink">
                      <span data-testid={`spec-name-${s.specId}`}>{s.name}</span>
                      {defaultSpecId === s.specId && (
                        <span
                          data-testid={`spec-default-${s.specId}`}
                          className="ml-2 rounded border border-accent/60 bg-accent/10 px-1 text-[10px] text-accent-soft"
                          title={t('检查/处理入口默认选这条（界面偏好）')}
                        >
                          {t('默认')}
                        </span>
                      )}
                      {s.enabled === 0 && (
                        <span className="ml-2 rounded border border-line px-1 text-[10px] text-ink-dim">{t('已停用')}</span>
                      )}
                      {s.originKey && (
                        <span className="ml-2 text-[10px] text-ink-dim" title={t('迁移来源 {key}', { key: s.originKey })}>
                          {s.originKey.startsWith('legacy-pack:')
                            ? t('旧包迁移（{key}）', { key: s.originKey.slice(12) })
                            : t('官方自带')}
                        </span>
                      )}
                      {!s.pluginStatus.installed && s.pluginStatus.note && (
                        <div className="text-[10px] text-amber-300" data-testid={`spec-note-${s.specId}`}>
                          {s.pluginStatus.note}
                        </div>
                      )}
                      {s.pluginStatus.versionMismatch && (
                        <div className="text-[10px] text-amber-300" data-testid={`spec-mismatch-${s.specId}`}>
                          {t('方案钉在版本 {pin}，当前装的是 {ver}', {
                            pin: s.pluginVersionPin ?? '',
                            ver: s.pluginStatus.version ?? '',
                          })}
                        </div>
                      )}
                    </td>
                    <td className="py-1 pr-2 text-ink-dim">
                      {s.rules.length === 0 ? t('全部') : t('{n} 条', { n: s.rules.length })}
                    </td>
                    <td className="py-1 pr-2 text-ink-dim">{changed === 0 ? t('无（跟默认设置）') : t('{n} 项', { n: changed })}</td>
                    <td className="py-1 text-right">
                      <Button
                        type="button"
                        variant="secondary"
                        size="none"
                        className="px-1.5 py-0.5 text-[11px]"
                        data-testid={`spec-edit-${s.specId}`}
                        onClick={() => setEditor({ spec: s })}
                      >
                        {t('编辑')}
                      </Button>
                      <Button
                        type="button"
                        variant="secondary"
                        size="none"
                        className="ml-1 px-1.5 py-0.5 text-[11px]"
                        data-testid={`spec-default-set-${s.specId}`}
                        disabled={defaultSpecId === s.specId}
                        onClick={() => makeDefault(s)}
                      >
                        {t('设为默认')}
                      </Button>
                      <Button
                        type="button"
                        variant="secondary"
                        size="none"
                        tone="danger"
                        className="ml-1 px-1.5 py-0.5 text-[11px]"
                        data-testid={`spec-delete-${s.specId}`}
                        disabled={busy !== null}
                        onClick={() => void removeSpec(s)}
                      >
                        {t('删除')}
                      </Button>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        )}
      </section>

      {/* ------------------------------------------------ ③ 默认设置（只读） */}
      <section className="mt-3 rounded-lg border border-line bg-panel p-3" data-testid="plugin-section-params">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-xs font-semibold text-ink">{t('默认设置')}</h2>
          <span className="text-[11px] text-ink-dim" data-testid="plugin-defaults-note">
            {t('官方推荐参数，不可改；想改请另存为自己的方案')}
          </span>
          {!installed && schema && (
            <Link
              to="/settings?drawer=packs"
              data-testid="plugin-params-install-link"
              className="rounded border border-amber-600/60 bg-amber-600/10 px-1.5 py-0.5 text-[10px] text-amber-300 hover:underline"
              title={t('去设置的包管理面装包，装好后这里就能用')}
            >
              {t('装好后可用 · 去设置装包')}
            </Link>
          )}
        </div>
        {schema ? (
          <div className="mt-2">
            <ParamForm
              schema={schema}
              mode={installed ? 'defaults' : 'disabled'}
              testId="plugin-paramform"
            />
          </div>
        ) : (
          <p className="mt-2 rounded border border-line bg-panel-2 px-2 py-1 text-[11px] text-ink-dim" data-testid="plugin-params-undeclared">
            {schemaView && !schemaView.declared
              ? t('这个能力没有可调参数（老包：参数按官方默认直接用）。')
              : t('读不到参数声明：{err}', { err: schemaErr ?? t('能力未安装') })}
          </p>
        )}
      </section>

      {/* ------------------------------------------------ 安装 / 版本 / 装载态（唯一一处） */}
      <section className="mt-3 rounded-lg border border-line bg-panel p-3" data-testid="plugin-section-versions">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-xs font-semibold text-ink">{t('安装与版本')}</h2>
          <span className="text-[11px] text-ink-dim">
            {t('同 id 可并存多版本，只有一个是当前版本：切版本/回滚只挪指针，卸载才删目录')}
          </span>
          {hasLedger && !rp.builtin && (
            <Button
              type="button"
              variant="secondary"
              size="none"
              className="px-1.5 py-0.5 text-[11px]"
              data-testid="plugin-rollback"
              disabled={busy !== null || present.length < 2}
              title={
                present.length < 2
                  ? t('至少要装过两个版本才能回滚')
                  : t('切到上一个已装版本（旧版本目录保留，可再切回）')
              }
              onClick={() => void switchVersion(undefined, 'rollback')}
            >
              {t('回滚到上一版')}
            </Button>
          )}
        </div>

        {!hasLedger && present.length > 0 ? (
          /* 内置包：没有账本行 = 没有版本栈（切版本/回滚/卸载都不适用），只把「当前生效的那份」说清 */
          <table className="mt-2 w-full text-[11px]">
            <thead className="text-ink-dim">
              <tr>
                <th className="py-0.5 text-left font-medium">{t('版本')}</th>
                <th className="py-0.5 text-left font-medium">{t('目录')}</th>
                <th className="py-0.5 text-left font-medium">API</th>
                <th className="py-0.5 text-left font-medium">{t('确定性')}</th>
                <th className="py-0.5 text-left font-medium">{t('签名')}</th>
              </tr>
            </thead>
            <tbody>
              {versions.map((v) => (
                <tr key={v.version} className="border-t border-line" data-testid={`plugin-version-${pluginId}-${v.version}`}>
                  <td className="py-0.5 pr-2 text-ink">
                    {v.version}
                    <span
                      data-testid={`plugin-version-current-${pluginId}-${v.version}`}
                      className="ml-2 rounded border border-emerald-600/60 bg-emerald-600/10 px-1 text-[10px] text-emerald-300"
                    >
                      {t('当前（内置）')}
                    </span>
                  </td>
                  <td className="max-w-[16rem] truncate py-0.5 pr-2 text-ink-dim" title={v.dir}>
                    {v.dir || '—'}
                  </td>
                  <td className="py-0.5 pr-2 text-ink-dim">{v.apiVersion ?? '—'}</td>
                  <td className="py-0.5 pr-2 text-ink-dim">
                    {v.deterministic == null ? '—' : v.deterministic ? t('确定性') : t('含 VL')}
                  </td>
                  <td className="py-0.5 pr-2">
                    <span
                      data-testid={`plugin-sig-${pluginId}-${v.version}`}
                      className={`rounded border px-1 text-[10px] ${sigClass(v.signatureStatus)}`}
                    >
                      {sigLabel(v.signatureStatus)}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : present.length === 0 ? (
          <p className="mt-2 rounded border border-line bg-panel-2 px-2 py-1 text-[11px] text-ink-dim" data-testid="plugin-not-installed">
            {t('未安装任何版本（包目录不在）：装入 .dampack 后才可用；方案与参数声明照旧可见。')}
          </p>
        ) : (
          <>
            {rp.staleCurrent && (
              <p
                className="mt-2 rounded border border-amber-600/60 bg-amber-600/10 px-2 py-1 text-[11px] text-amber-300"
                data-testid="plugin-stale"
              >
                {t('当前指针失效：')}<code>{rp.staleCurrent}</code>{t(' 的目录已被删除 → 重装该版本，或把别的已装版本设为当前。')}
              </p>
            )}
            <table className="mt-2 w-full text-[11px]">
              <thead className="text-ink-dim">
                <tr>
                  <th className="py-0.5 text-left font-medium">{t('版本')}</th>
                  <th className="py-0.5 text-left font-medium">{t('目录')}</th>
                  <th className="py-0.5 text-left font-medium">API</th>
                  <th className="py-0.5 text-left font-medium">{t('确定性')}</th>
                  <th className="py-0.5 text-left font-medium">{t('签名')}</th>
                  <th className="py-0.5 text-right font-medium">{t('操作')}</th>
                </tr>
              </thead>
              <tbody>
                {versions.map((v) => (
                  <tr key={v.version} className="border-t border-line" data-testid={`plugin-version-${pluginId}-${v.version}`}>
                    <td className="py-0.5 pr-2 text-ink">
                      {v.version}
                      {v.current && (
                        <span
                          data-testid={`plugin-version-current-${pluginId}-${v.version}`}
                          className="ml-2 rounded border border-emerald-600/60 bg-emerald-600/10 px-1 text-[10px] text-emerald-300"
                        >
                          {t('当前')}
                        </span>
                      )}
                      {!v.present && (
                        <span
                          data-testid={`plugin-version-absent-${pluginId}-${v.version}`}
                          className="ml-2 rounded border border-line px-1 text-[10px] text-ink-dim"
                          title={t('账本行保留审计，包目录已删除')}
                        >
                          {t('目录已删')}
                        </span>
                      )}
                    </td>
                    <td className="max-w-[16rem] truncate py-0.5 pr-2 text-ink-dim" title={v.dir}>
                      {v.dir}
                    </td>
                    <td className="py-0.5 pr-2 text-ink-dim">{v.apiVersion ?? '—'}</td>
                    <td className="py-0.5 pr-2 text-ink-dim">
                      {v.deterministic == null ? '—' : v.deterministic ? t('确定性') : t('含 VL')}
                    </td>
                    <td className="py-0.5 pr-2">
                      <span
                        data-testid={`plugin-sig-${pluginId}-${v.version}`}
                        className={`rounded border px-1 text-[10px] ${sigClass(v.signatureStatus)}`}
                        title={
                          v.signatureStatus === 'invalid'
                            ? t('签名与信任公钥不符：该版本不应被装载')
                            : t('manifest 里的签名状态快照')
                        }
                      >
                        {sigLabel(v.signatureStatus)}
                      </span>
                    </td>
                    <td className="py-0.5 text-right">
                      <Button
                        type="button"
                        variant="secondary"
                        size="none"
                        className="px-1.5 py-0.5 text-[11px]"
                        data-testid={`plugin-setcurrent-${pluginId}-${v.version}`}
                        disabled={busy !== null || v.current || !v.present}
                        title={
                          !v.present ? t('目录已删除，无法切为当前') : v.current ? t('已是当前版本') : t('把当前指针切到这个版本')
                        }
                        onClick={() => void switchVersion(v.version, 'activate')}
                      >
                        {t('设为当前')}
                      </Button>
                      <Button
                        type="button"
                        variant="secondary"
                        size="none"
                        tone="danger"
                        className="ml-1 px-1.5 py-0.5 text-[11px]"
                        data-testid={`plugin-uninstall-${pluginId}-${v.version}`}
                        disabled={busy !== null || rp.builtin || !v.present}
                        title={
                          rp.builtin
                            ? t('内置包随产品分发，不可卸载（升级 = 装新版本再切当前）')
                            : !v.present
                              ? t('目录已删除，无可卸载')
                              : t('删除该版本包目录（账本行保留审计；方案与历史结论不动）')
                        }
                        onClick={() => void uninstall(v.version)}
                      >
                        {t('卸载')}
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </>
        )}

        <PackInstaller
          hostCaps={hostCaps}
          expectPluginId={pluginId}
          submitLabel={versions.length > 0 ? t('装这个能力的新版本') : t('安装 .dampack')}
          hint={
            policy?.required
              ? t('安装期强制验签（{n} 把信任公钥）', { n: policy.trustedKeys })
              : t('未配信任公钥：装包不做验签')
          }
          onInstalled={() => void refresh()}
        />

        <p className="mt-2 text-[11px] leading-5 text-ink-dim">
          {t(
            '装载态只反映 manifest 能不能读；子进程的 running / 冷却（默认 300s）/ 崩溃 是执行时刻的运行态，失败会以「未判定 + plugin_timeout / plugin_crash / plugin_unavailable」出现在处理记录里。',
          )}
        </p>
      </section>

      {/* 批量结果与进度不在这里（一个能力只归一个区域）：只给出口 */}
      <p className="mt-3 text-[11px] text-ink-dim" data-testid="plugin-detail-where">
        {t('批量进度、历史结果、失败明细不在此页，都在')}
        <Link to={`/tasks?pack=${encodeURIComponent(pluginId)}`} className="mx-1 text-accent hover:underline">
          {t('处理记录')}
        </Link>
        {t('（执行进度 / 失败明细 / 结论与改判）。装在哪个目录、标准清单原文见上方「能力说明」与安装回执。')}
      </p>

      <div className="mt-2 text-[11px] text-ink-dim">
        <Link to="/settings?drawer=packs" className="text-accent hover:underline">
          {t('← 回设置 · 包管理（换包 / 导入 / 装新能力）')}
        </Link>
      </div>

      {editor && (
        <SpecEditorDialog
          open
          onClose={() => setEditor(null)}
          pluginId={pluginId}
          pluginName={pluginName}
          schema={schema}
          availableRules={rules}
          spec={editor.spec}
          installed={installed}
          onSaved={(saved) => {
            setSpecs((cur) => {
              const hit = cur.findIndex((s) => s.specId === saved.specId)
              if (hit < 0) return [...cur, saved]
              const next = [...cur]
              next[hit] = saved
              return next
            })
            setDefaultId((cur) => cur ?? pickDefaultSpecId(pluginId, [saved]))
          }}
        />
      )}
    </div>
  )
}

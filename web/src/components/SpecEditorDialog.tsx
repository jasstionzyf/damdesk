/**
 * 规格编辑（新建 / 修改）—— 同一个 `ParamForm`，不改插件默认值。
 *
 * 口径（`plugin-design-and-ui.md` §4/§5、`plugin-param-schema.md` §9）：
 * - 规格 = 选哪些规则 + 阈值 + 动作链 + 输出 + 参数 diff；**默认值永远在插件里**，
 *   这里只写用户改过的那几项（`paramsDiff`）；
 * - 阈值/动作链/输出本卡只**回显**（旧行业包迁移过来的原文不许静默丢），编辑入口不在这里：
 *   阈值的新形态就是参数（`paramsDiff`），动作链编辑另卡；
 * - 插件卸载/升级不删规格：插件不在时照样能改（`pluginStatus.note` 会说明「装好后再执行」）。
 */
import { useEffect, useState } from 'react'
import { api, type ComplianceSpecView, type ParamSchemaDoc } from '../lib/api'
import { useToast } from '../lib/toast'
import { Button } from './Button'
import { ParamForm } from './ParamForm'
import { Dialog } from '../lib/modal'
import { useI18n } from '../i18n'

export function SpecEditorDialog({
  open,
  onClose,
  pluginId,
  pluginName,
  schema,
  availableRules,
  spec,
  installed,
  onSaved,
}: {
  open: boolean
  onClose: () => void
  pluginId: string
  pluginName: string
  /** 插件声明（`declared:false` → 不给参数区，说明老包参数原样透传） */
  schema: ParamSchemaDoc | null
  /** 包声明的规则 id（空 = 无法校验，就自由填/全选） */
  availableRules: string[]
  /** null = 新建 */
  spec: ComplianceSpecView | null
  installed: boolean
  onSaved: (spec: ComplianceSpecView) => void
}) {
  const { toast } = useToast()
  const { t } = useI18n()
  const [name, setName] = useState('')
  const [rules, setRules] = useState<string[]>([])
  const [diff, setDiff] = useState<Record<string, unknown>>({})
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  // 每次打开按传入的规格重置（Dialog 关闭不卸载 → 必须显式重置，否则残留上一条的改动）
  useEffect(() => {
    if (!open) return
    setName(spec?.name ?? '')
    setRules(spec?.rules ?? [])
    setDiff(spec?.paramsDiff ?? {})
    setErr(null)
  }, [open, spec])

  const toggleRule = (id: string) => {
    setRules((cur) => (cur.includes(id) ? cur.filter((r) => r !== id) : [...cur, id]))
  }

  const save = async () => {
    const trimmed = name.trim()
    if (!trimmed) {
      setErr(t('方案名不能为空（名字要能认出来是给谁用的，如「淘宝主图」）'))
      return
    }
    setBusy(true)
    setErr(null)
    try {
      const saved = spec
        ? await api.specUpdate(spec.specId, { name: trimmed, rules, paramsDiff: diff })
        : await api.specCreate({ pluginId, name: trimmed, rules, paramsDiff: diff })
      toast(
        'success',
        spec ? t('方案已更新') : t('方案已新建'),
        t('{name} · {rules} · {n} 项参数改动', {
          name: saved.name,
          rules: saved.rules.length === 0 ? t('全部规则') : t('{n} 条规则', { n: saved.rules.length }),
          n: Object.keys(saved.paramsDiff).length,
        }),
      )
      onSaved(saved)
      onClose()
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const rulePool = availableRules.length > 0 ? availableRules : []
  const extraPicked = rules.filter((r) => !rulePool.includes(r))

  return (
    <Dialog
      open={open}
      onClose={onClose}
      testId="spec-editor"
      title={spec ? t('改方案：{name}', { name: spec.name }) : t('新建方案')}
      subtitle={t('{name}（{id}）· 方案只存你改过的那几项，官方默认设置不动', { name: pluginName, id: pluginId })}
      widthClass="max-w-2xl"
      footer={
        <>
          <Button type="button" variant="secondary" onClick={onClose} data-testid="spec-editor-cancel">
            {t('取消')}
          </Button>
          <Button type="button" onClick={() => void save()} disabled={busy} data-testid="spec-editor-save">
            {busy ? t('保存中…') : spec ? t('保存方案') : t('创建方案')}
          </Button>
        </>
      }
    >
      {!installed && (
        <p className="mb-2 rounded border border-amber-600/60 bg-amber-600/10 px-2 py-1 text-[11px] text-amber-300">
          {t('这个能力当前没装上：方案照样可以改（方案是你的，卸载/升级都不删），装好后再执行。')}
        </p>
      )}

      <label className="block text-[11px] text-ink-dim" htmlFor="spec-name">
        {t('方案名（如 淘宝主图 / 亚马逊主图 / 我的 800px webp）')}
      </label>
      <input
        id="spec-name"
        value={name}
        onChange={(e) => setName(e.target.value)}
        data-testid="spec-name"
        placeholder={t('如：淘宝主图 800px webp')}
        className="mt-1 w-full rounded border border-line bg-panel-2 px-2 py-1 text-xs text-ink outline-none focus:border-accent"
      />

      <h3 className="mt-3 text-[11px] font-medium text-ink-dim">
        {t('选规则（不勾 = 用官方全部规则；换平台/换标准 = 新建一条方案）')}
      </h3>
      {rulePool.length === 0 ? (
        <p className="mt-1 text-[11px] text-ink-dim" data-testid="spec-rules-unknown">
          {t('（包没声明规则清单 / 未安装：这一项无法校验，留空表示「全部规则」）')}
        </p>
      ) : (
        <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1" data-testid="spec-rules">
          {rulePool.map((r) => (
            <label key={r} className="flex items-center gap-1 text-[11px] text-ink-dim">
              <input
                type="checkbox"
                data-testid={`spec-rule-${r}`}
                checked={rules.includes(r)}
                onChange={() => toggleRule(r)}
              />
              <code className="text-ink">{r}</code>
            </label>
          ))}
          {extraPicked.map((r) => (
            <label key={r} className="flex items-center gap-1 text-[11px] text-amber-300">
              <input
                type="checkbox"
                data-testid={`spec-rule-${r}`}
                checked
                onChange={() => toggleRule(r)}
              />
              <code>{r}</code>
              <span>{t('（包未声明：保存时宿主会拒）')}</span>
            </label>
          ))}
        </div>
      )}

      <h3 className="mt-3 text-[11px] font-medium text-ink-dim">{t('参数（只存改动）')}</h3>
      {schema ? (
        <div className="mt-1 rounded border border-line bg-panel-2 p-2">
          <ParamForm schema={schema} value={diff} onChange={setDiff} mode="edit" testId="spec-paramform" />
        </div>
      ) : (
        <p className="mt-1 text-[11px] text-ink-dim" data-testid="spec-params-undeclared">
          {t('这个能力没有参数面板（老包：参数按协议原样传，没有可改的项）。')}
        </p>
      )}

      {/* 阈值/动作链/输出：旧行业包迁移过来的原文，本卡只回显不编辑（不许静默丢） */}
      {spec && (Object.keys(spec.thresholds ?? {}).length > 0 || (spec.actions ?? []).length > 0 || Object.keys(spec.output ?? {}).length > 0) && (
        <div className="mt-3 rounded border border-line bg-panel-2 p-2 text-[11px] text-ink-dim" data-testid="spec-legacy-config">
          <span className="text-ink">{t('随方案保存的原有配置（只读回显）')}</span>
          {Object.keys(spec.thresholds ?? {}).length > 0 && (
            <div className="mt-1">
              {t('阈值：')}<code className="break-all">{JSON.stringify(spec.thresholds)}</code>
            </div>
          )}
          {(spec.actions ?? []).length > 0 && (
            <div className="mt-1">
              {t('动作链：')}<code className="break-all">{JSON.stringify(spec.actions)}</code>
            </div>
          )}
          {Object.keys(spec.output ?? {}).length > 0 && (
            <div className="mt-1">
              {t('输出：')}<code className="break-all">{JSON.stringify(spec.output)}</code>
            </div>
          )}
        </div>
      )}

      {err && (
        <p
          className="mt-2 rounded border border-rose-600/60 bg-rose-600/10 px-2 py-1 text-[11px] text-rose-300"
          data-testid="spec-editor-error"
        >
          {err}
        </p>
      )}
    </Dialog>
  )
}

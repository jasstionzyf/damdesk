import { useCallback, useEffect, useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import {
  api,
  type ComplianceInstalledPluginPack,
  type ComplianceSignaturePolicyView,
} from '../lib/api'
import { pickI18nOr, usePackTextLocale } from '../i18n/pickI18n'
import { useToast } from '../lib/toast'
import { Button } from './Button'
import { PackInstaller } from './PackInstaller'
import { pluginList, runtimeOf } from '../lib/plugins'
import { capabilityPath, notifyPacksChanged } from '../lib/capabilityNav'
import { useI18n } from '../i18n'

/**
 * C6 处理插件管理（设置页入口 → `?drawer=packs` Drawer 内嵌）：清单 + 首次安装 + 导航。
 *
 * 语义（与后端一致，别在界面层发明）：
 * - 一个行业的合规检测/处理 = 一个插件，**自包含**（规则、词表、报告元数据都在插件包内）；
 * - 同一插件可并存多个版本：切 current / 回滚只挪指针，旧版本目录保留，卸载才是删目录；
 * - 内置插件随产品分发，**不可卸载**（升级 = 装新版本再切当前）；
 * - 装载失败的插件照样列出并显示原因（坏包不能静默消失，否则用户以为"没装上"）。
 *
 * 分工（N链合流 N4 × N18）：**本页只做清单 + 首次安装 + 导航** —— 能力说明 / 我的方案 /
 * 参数默认值 / 切版本 / 回滚 / 卸载全在 `/plugins/<id>` 详情页（一个能力只归一个区域）；
 * K16/N18 之后不再有「YAML 配置包 / 激活行业包 / 合规引擎开关」这套东西，判定默认走插件。
 */

/** 版本排序 / 签名词 / 装载态词统一来自 `lib/plugins.ts`（列表页与详情页共用一份实现） */

/** 版本排序 / 签名词 / 装载态词统一来自 `lib/plugins.ts`（列表页与详情页共用一份实现） */

export function PackManager() {
  const locale = usePackTextLocale()
  const { toast } = useToast()
  const navigate = useNavigate()
  const { t } = useI18n()
  const [err, setErr] = useState<string | null>(null)
  // IP-K08/K09：代码插件包（.dampack）—— 本页只有清单与首次安装，其余动作在详情页。
  // 存原始 packs 而不是 `plugins[]`：内置包随产品分发、**不进账本**，只看账本会把
  // 仓库自带的电商包显示成「没装」（见 lib/plugins.pluginList 的合并口径）。
  const [packs, setPacks] = useState<{
    plugins?: ComplianceInstalledPluginPack[]
  }>({})
  /** 插件 manifest 的领域名（列表主标签用它，id 退为次要标识；拿不到就只显示 id） */
  const [policy, setPolicy] = useState<ComplianceSignaturePolicyView | null>(null)
  const [hostCaps, setHostCaps] = useState<string[]>([])

  const refresh = useCallback(async () => {
    setErr(null)
    try {
      const r = await api.compliancePacks()
      setPacks({ plugins: r.plugins })
      setPolicy(r.signaturePolicy ?? null)
      setHostCaps(r.hostCapabilities ?? [])
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
    }
  }, [])

  useEffect(() => {
    void refresh()
  }, [refresh])

  /** 清单行 = 账本 + 内置包身份（去重）：漏掉后者，仓库自带包会显示成「没装」 */
  const rows = pluginList(packs)

  return (
    <section className="rounded-lg border border-line bg-panel p-4" data-testid="pack-manager-plugins">
      <h2 className="text-sm font-semibold">{t('处理插件')}</h2>
      <p className="mt-1 text-xs leading-5 text-ink-dim">
        {t('能做什么 =')}{' '}
        <strong className="text-ink">{t('插件')}</strong>
        {t('（规则/算法 + 默认参数），要做成什么样 =')}{' '}
        <strong className="text-ink">{t('方案')}</strong>
        {t('（选规则 + 阈值 + 动作链 + 输出，在插件详情页建）。规则、词表、报告元数据都在插件包（')}
        <code>.dampack</code>
        {t(
          ' = manifest.json + plugin/ + resources/）里，自包含、随插件版本走。同一插件可并存多个版本，只有一个是当前版本：切版本/回滚都只挪指针，旧版本目录保留；卸载才是删目录。判定历史按插件版本记（换版本不删判定）。审核与反馈见',
        )}{' '}
        <Link to="/tasks" className="text-accent hover:underline">
          {t('任务中心')}
        </Link>
        {t('。')}
      </p>

      {err && (
        <p className="mt-2 rounded border border-rose-600/60 bg-rose-600/10 px-2 py-1 text-xs text-rose-300">
          {t('读取插件清单失败：{err}', { err })}
        </p>
      )}

      {/* 签名策略 + 本部署授权能力：装包前先让用户知道门禁在哪 */}
      <div className="mt-2 flex flex-wrap items-center gap-2 text-[11px]" data-testid="pack-policy-row">
        <span
          data-testid="pack-signature-policy"
          className={`rounded border px-2 py-0.5 ${
            policy?.required
              ? 'border-emerald-600/60 bg-emerald-600/10 text-emerald-300'
              : 'border-amber-600/60 bg-amber-600/10 text-amber-300'
          }`}
          title={
            policy?.required
              ? t('安装了信任公钥：验签不过 / 签名缺失的包会被拒绝安装')
              : t('没有信任公钥：装包不做验签（manifest 里的签名只被记为 declared/unsigned）')
          }
        >
          {t('安装期签名策略：')}
          {policy?.required
            ? t('强制验签（{n} 把信任公钥）', { n: policy.trustedKeys })
            : t('不验签（未配信任公钥）')}
        </span>
        <span className="flex flex-wrap items-center gap-1 text-ink-dim" data-testid="pack-host-caps">
          {t('本部署授权能力：')}
          {hostCaps.length === 0 ? (
            <span className="text-ink-dim">{t('（无）')}</span>
          ) : (
            hostCaps.map((c) => (
              <code key={c} className="rounded border border-line bg-panel-2 px-1">
                {c}
              </code>
            ))
          )}
        </span>
      </div>

      {rows.length === 0 && (
        <p className="mt-3 rounded border border-line bg-panel-2 px-2 py-1 text-xs text-ink-dim" data-testid="pack-plugins-empty">
          {t('还没装任何处理插件。安装 .dampack 后判定由插件执行；未安装时判定统一落「未判定 + plugin_unavailable」。')}
        </p>
      )}

      {/* 插件清单：只做导航（详情/方案/参数/版本动作都在 /plugins/<id>） */}
      {rows.length > 0 && (
        <table className="mt-3 w-full text-[11px]">
          <thead className="text-ink-dim">
            <tr>
              <th className="py-0.5 text-left font-medium">{t('插件')}</th>
              <th className="py-0.5 text-left font-medium">{t('来源')}</th>
              <th className="py-0.5 text-left font-medium">{t('当前版本')}</th>
              <th className="py-0.5 text-left font-medium">{t('装载态')}</th>
              <th className="py-0.5 text-right font-medium">{t('操作')}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((pack) => {
              const rt = runtimeOf(
                pack.runtime ?? {
                  available: false,
                  reason: 'plugin_pack_missing',
                  note: t('包目录不在'),
                },
              )
              return (
                <tr
                  key={pack.id}
                  className="border-t border-line"
                  data-testid={`plugin-pack-${pack.id}`}
                >
                  <td className="py-1 pr-2 text-ink">
                    <span className="font-medium text-ink" data-testid={`plugin-name-${pack.id}`}>
                      {pickI18nOr(pack.nameI18n, pack.name, locale) || pack.id}
                    </span>
                    {pack.name && <span className="text-ink-dim">{pack.id}</span>}
                    {pack.staleCurrent && (
                      <div className="text-[10px] text-amber-300" data-testid={`plugin-stale-${pack.id}`}>
                        {t('当前指针失效（{id} 的目录已删）→ 去详情页重装或切版本', { id: pack.staleCurrent })}
                      </div>
                    )}
                    {pack.versions.length === 0 && (
                      <div className="text-[10px] text-ink-dim" data-testid={`plugin-not-installed-${pack.id}`}>
                        {t('未安装任何版本（包目录不在）')}
                      </div>
                    )}
                  </td>
                  <td className="py-1 pr-2">
                    <span
                      data-testid={`plugin-builtin-${pack.id}`}
                      className={`rounded border px-1.5 py-0.5 text-[10px] ${
                        pack.builtin
                          ? 'border-accent/60 bg-accent/10 text-accent-soft'
                          : 'border-line text-ink-dim'
                      }`}
                      title={pack.builtin ? t('随产品分发的内置包（可升级/切版本，不可卸载）') : t('第三方安装包')}
                    >
                      {pack.builtin ? t('内置') : t('第三方')}
                    </span>
                  </td>
                  <td className="py-1 pr-2 text-ink" data-testid={`plugin-current-${pack.id}`}>
                    {pack.currentVersion ?? '—'}
                  </td>
                  <td className="py-1 pr-2">
                    <span
                      data-testid={`plugin-runtime-${pack.id}`}
                      title={rt.hint}
                      className={`rounded border px-1.5 py-0.5 text-[10px] ${rt.cls}`}
                    >
                      {t('装载态 {word}', { word: rt.word })}
                    </span>
                  </td>
                  <td className="py-1 text-right">
                    {/* 详情 = 能力说明 / 我的方案 / 参数默认值 / 切版本·回滚·卸载（唯一入口） */}
                    <Button
                      type="button"
                      data-testid={`plugin-detail-${pack.id}`}
                      title={t('插件详情：能力说明 + 我的方案 + 参数默认值 + 安装与版本（切版本/回滚/卸载都在那一页）')}
                      onClick={() => navigate(capabilityPath(pack.id))}
                      variant="secondary"
                      size="none"
                      className="px-1.5 py-0.5 text-[11px]"
                    >
                      {t('详情')}
                    </Button>
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      )}

      {/* 首次安装：装一个还没有详情页的插件（已有插件的「装新版本」在它自己的详情页里） */}
      <PackInstaller
        hostCaps={hostCaps}
        submitLabel={t('安装新插件（.dampack）')}
        onInstalled={async (r) => {
          // 不自动跳走：回执（验签/摘要/能力）要留在这一屏可复查；装完清单里就有这行「详情」
          await refresh()
          // KI-35 P1：装好 = 主导航能力区立即多一项（同一个注册表，事件触发重拉）
          notifyPacksChanged()
          toast(
            'success',
            t('插件已就绪'),
            t('{id}@{ver}：点这一行的「详情」配置方案与参数', { id: r.id, ver: r.version }),
          )
        }}
      />

      <p className="mt-1 text-[11px] leading-5 text-ink-dim">
        {t(
          '不勾「装完立刻切」时，只有该包还没有当前版本才会自动切（补装旧版本不抢指针）。装载态只反映 manifest 能不能读；子进程的 running / 冷却（默认 300s）/ 崩溃 是判定时刻的运行态，失败会以「未判定 + plugin_timeout / plugin_crash / plugin_unavailable」出现在任务中心这次执行的明细里。',
        )}
      </p>
    </section>
  )
}

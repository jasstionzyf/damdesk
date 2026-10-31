/**
 * 装 `.dampack` 的**唯一实现**（两处挂载，各自的职责不同、不重复）：
 *
 * - 行业包管理（`PackManager`）= **首次安装一个还没详情页的插件**（列表页是入口，装完才有
 *   `/plugins/<id>` 可看）；
 * - 插件详情页 = **给这个插件再装一个版本**（升级 / 补装旧版本）。
 *
 * 界面相同、接口相同、回执相同 —— 所以只写一份。回执必须留在界面上（验签结果 / 签名策略 /
 * 摘要核对 / 能力声明），**不能一闪而过的 toast**：验签失败是「装上了但不能信」的状态，
 * 用户要能复查。
 */
import { useCallback, useState } from 'react'
import { api, type CompliancePackInstallResult } from '../lib/api'
import { useToast } from '../lib/toast'
import { Button } from './Button'
import { sigLabel } from '../lib/plugins'
import { useI18n } from '../i18n'

export function PackInstaller({
  hostCaps,
  onInstalled,
  expectPluginId,
  submitLabel,
  hint,
}: {
  /** 本部署授权能力（用于把「包声明了但这里没授权」提前说清，不让用户撞 403） */
  hostCaps: string[]
  onInstalled?: (r: CompliancePackInstallResult) => void
  /** 详情页挂载时限定插件 id：装进来的包不属于这个页面 → 明确报错，不默默换页 */
  expectPluginId?: string
  submitLabel?: string
  hint?: string
}) {
  const { toast } = useToast()
  const { t } = useI18n()
  const [archive, setArchive] = useState<{ name: string; base64: string } | null>(null)
  const [activate, setActivate] = useState(false)
  const [busy, setBusy] = useState(false)
  const [receipt, setReceipt] = useState<CompliancePackInstallResult | null>(null)

  /** 选文件 → 读成 base64（`.dampack` 是 zip；浏览器只能上传字节，服务端同机路径那条口不再开） */
  const pick = useCallback(
    (file: File | null) => {
      if (!file) {
        setArchive(null)
        return
      }
      const reader = new FileReader()
      reader.onerror = () => toast('error', t('读不到这个文件'), file.name)
      reader.onload = () => {
        const bytes = new Uint8Array(reader.result as ArrayBuffer)
        // 分块拼二进制串：一次 spread 整个大文件会爆调用栈
        let bin = ''
        const CHUNK = 0x8000
        for (let i = 0; i < bytes.length; i += CHUNK) {
          bin += String.fromCharCode(...bytes.subarray(i, i + CHUNK))
        }
        setArchive({ name: file.name, base64: btoa(bin) })
      }
      reader.readAsArrayBuffer(file)
    },
    [toast],
  )

  const doInstall = async () => {
    if (!archive) {
      toast('warning', t('先选一个 .dampack 文件'), t('归档里要有 manifest.json + plugin/ + resources/（可选 README.md）'))
      return
    }
    setBusy(true)
    try {
      const r = await api.compliancePackInstall({
        archiveBase64: archive.base64,
        filename: archive.name,
        activate: activate || undefined,
      })
      setReceipt(r)
      if (expectPluginId && r.id !== expectPluginId) {
        toast(
          'warning',
          t('装的是另一个插件'),
          t('这个文件是 {id}@{ver}，不是当前页面的 {expect} —— 去它的详情页看版本。', {
            id: r.id,
            ver: r.version,
            expect: expectPluginId,
          }),
        )
      } else {
        const sig = r.signaturePolicy.required
          ? t('强制验签（{n} 把信任公钥）· {sig}', {
              n: r.signaturePolicy.trustedKeys,
              sig: sigLabel(r.signatureStatus),
            })
          : t('未配信任公钥 · {sig}', { sig: sigLabel(r.signatureStatus) })
        // 签名无效是**装上了但不能信**的状态：必须响亮，不能只当成功回执的注脚
        if (r.signatureStatus === 'invalid') {
          toast(
            'error',
            t('已装载但签名验不过'),
            t('{id}@{ver} · {sig} · 该包不应被用于正式判定', { id: r.id, ver: r.version, sig }),
          )
        } else {
          toast(
            'success',
            r.reused ? t('该版本已装过（内容相同，未新建副本）') : t('插件包已安装'),
            t('{id}@{ver} · api_version {api} · {sig}', { id: r.id, ver: r.version, api: r.apiVersion, sig }) +
              (r.digestsChecked ? t(' · SHA256SUMS 逐文件核对通过') : '') +
              (r.activated ? t(' · 已切为当前版本') : ''),
          )
        }
      }
      setArchive(null)
      onInstalled?.(r)
    } catch (e) {
      toast('error', t('安装失败'), e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <div className="mt-3 flex flex-wrap items-center gap-2 text-[11px]">
        <label className="flex items-center gap-2 text-ink-dim">
          {t('安装包')}
          <input
            type="file"
            accept=".dampack,.zip,application/zip"
            data-testid="plugin-install-file"
            onChange={(ev) => pick(ev.target.files?.[0] ?? null)}
            className="max-w-64 rounded border border-line bg-panel-2 px-1 py-0.5 text-xs text-ink-dim file:mr-2 file:rounded file:border-0 file:bg-panel file:px-2 file:py-0.5 file:text-ink"
          />
        </label>
        <span className="text-ink-dim" data-testid="plugin-install-picked">
          {archive
            ? t('{name}（{size}）', {
                name: archive.name,
                size:
                  archive.base64.length * 0.75 >= 1024
                    ? `${Math.round((archive.base64.length * 0.75) / 1024)} KB`
                    : `${Math.round(archive.base64.length * 0.75)} B`,
              })
            : t('未选择文件')}
        </span>
        <label className="flex items-center gap-1 text-ink-dim">
          <input
            type="checkbox"
            checked={activate}
            data-testid="plugin-install-activate"
            onChange={(ev) => setActivate(ev.target.checked)}
          />
          {t('装完立刻切为当前版本')}
        </label>
        <Button
          type="button"
          disabled={busy || !archive}
          data-testid="plugin-install"
          onClick={() => void doInstall()}
          variant="secondary"
        >
          {busy ? t('安装中…') : (submitLabel ?? t('安装 .dampack'))}
        </Button>
        {hint && <span className="text-ink-dim">{hint}</span>}
      </div>

      {receipt && (
        <div
          className={`mt-2 rounded border px-2 py-1.5 text-[11px] leading-5 ${
            receipt.signatureStatus === 'invalid'
              ? 'border-rose-600/60 bg-rose-600/10 text-rose-300'
              : 'border-line bg-panel-2 text-ink-dim'
          }`}
          data-testid="plugin-install-receipt"
        >
          <span className="text-ink">{t('最近一次安装')}</span>
          {t('：{id}@{ver} → {dir}', { id: receipt.id, ver: receipt.version, dir: receipt.dir })}
          <br />
          {t('签名：{sig}', { sig: sigLabel(receipt.signatureStatus) })}
          {receipt.signatureStatus === 'invalid' && (
            <span className="text-rose-300">{t('（验签失败：该包不应被用于正式判定）')}</span>
          )}
          {' · '}
          {t('策略：{policy}', {
            policy: receipt.signaturePolicy.required
              ? t('强制验签（{n} 把信任公钥）', { n: receipt.signaturePolicy.trustedKeys })
              : t('不验签（未配信任公钥）'),
          })}
          {' · '}
          {t('摘要核对：{v}', { v: receipt.digestsChecked ? t('SHA256SUMS 逐文件通过') : t('归档未带 SHA256SUMS（未核对）') })}
          {' · '}
          {receipt.reused ? t('同版本同内容已存在（未新建副本）') : t('新副本已落盘')}
          {receipt.activated ? t(' · 已切为当前版本') : ''}
          <br />
          {t('能力声明：')}
          {receipt.capabilities.length === 0 ? (
            <span className="text-rose-300">{t('（包没声明任何能力 = 装上也判不了）')}</span>
          ) : (
            receipt.capabilities.map((c) => (
              <code key={c} className="ml-1 rounded border border-line bg-panel px-1">
                {c}
              </code>
            ))
          )}
          {receipt.capabilities.some((c) => !hostCaps.includes(c)) && (
            <span className="ml-1 text-amber-300">{t('（其中有本部署未授权的能力 → 调用会 403）')}</span>
          )}
        </div>
      )}
    </>
  )
}

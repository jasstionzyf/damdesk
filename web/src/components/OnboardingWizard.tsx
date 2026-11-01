import { useState } from 'react'
import { api, formatSize, type ScanResult } from '../lib/api'
import { Dialog } from '../lib/modal'
import { isDesktopShell, pickFolder } from '../lib/native'
import { useToast } from '../lib/toast'
import { useI18n } from '../i18n'
import { Button } from './Button'

export interface ImportConfig {
  dir: string
  mode: 'inplace' | 'copyin'
  autoTag: boolean
}

/** 步骤名是中文 key，渲染点 t() 翻译（模块级不能持有 hook）。 */
const STEPS = ['选择目录', '导入方式', '处理选项'] as const

/**
 * 首启导入向导：扫描目录（真实 /api/import/scan）→ 原地/copy-in → 选项。
 *
 * 载体（UI-CARRIER-1）：**Dialog**（3 步向导 = 判据上限内：≤6 字段、单屏、无二级列表、结果同屏）。
 * 浮层 chrome 统一由 lib/modal.tsx 提供（aria-labelledby 指可见标题「欢迎使用 DAM」、背景 inert
 * + 滚动锁、Esc/✕/遮罩三通道、关闭归还焦点）；本组件只管步骤内容。
 */
export function OnboardingWizard({
  onClose,
  onFinish,
  open = true,
}: {
  onClose: () => void
  onFinish: (cfg: ImportConfig) => void
  open?: boolean
}) {
  const { t } = useI18n()
  const [step, setStep] = useState(0)
  const [dir, setDir] = useState('')
  const [mode, setMode] = useState<'inplace' | 'copyin'>('inplace')
  const [autoTag, setAutoTag] = useState(true)
  const [scanning, setScanning] = useState(false)
  const [picking, setPicking] = useState(false)
  const [scan, setScan] = useState<ScanResult | null>(null)
  const [scanErr, setScanErr] = useState<string | null>(null)
  const { toast } = useToast()

  // 桌面壳才有系统文件夹选择框；容器/浏览器形态保持手填路径（web 拿不到本地绝对路径）
  const desktop = useState(isDesktopShell)[0]
  const dirValid = dir.trim().length > 0
  const last = step === STEPS.length - 1

  /** 原生目录选择（仅桌面壳）：选中即回填并清掉上一次扫描结果 */
  const doPick = async () => {
    setPicking(true)
    setScanErr(null)
    try {
      const picked = await pickFolder()
      if (picked) {
        setDir(picked)
        setScan(null)
      }
    } catch (e) {
      setScanErr(t('打开文件夹选择框失败：{msg}', { msg: e instanceof Error ? e.message : String(e) }))
    } finally {
      setPicking(false)
    }
  }

  const doScan = () => {
    setScanning(true)
    setScanErr(null)
    setScan(null)
    api
      .scan(dir.trim())
      .then((r) => {
        setScan(r)
        if (r.imageFiles === 0) setScanErr(t('目录里没有图片文件（支持 jpg/png/webp/gif/bmp/avif/heic）'))
      })
      .catch((e) => setScanErr(e instanceof Error ? e.message : String(e)))
      .finally(() => setScanning(false))
  }

  const finish = () => {
    toast('success', t('导入已开始'), t('进度见右上角处理记录'))
    onFinish({ dir: dir.trim(), mode, autoTag })
  }

  return (
    <Dialog
      open={open}
      title={t('欢迎使用 DAM')}
      subtitle={t("三步完成首次导入：选择图片目录、决定存放方式、可选自动打标")}
      onClose={onClose}
      testId="onboarding-wizard"
      widthClass="max-w-[520px]"
      footer={
        <>
          <Button
            type="button"
            onClick={onClose}
            variant="secondary"
            size="md"
          >
            {t('稍后再说')}
          </Button>
          {step > 0 && (
            <Button
              type="button"
              onClick={() => setStep((s) => s - 1)}
              variant="secondary"
              size="md"
            >
              {t('上一步')}
            </Button>
          )}
          <Button
            type="button"
            disabled={step === 0 && (!dirValid || !scan || scan.imageFiles === 0)}
            onClick={() => (last ? finish() : setStep((s) => s + 1))}
            variant="primary"
            size="md"
          >
            {last ? t('开始导入') : t('下一步')}
          </Button>
        </>
      }
    >
      {/* 步骤条（Dialog 标题已在壳里，这里不重复 h2） */}
      <div className="flex items-center gap-2" data-testid="wizard-stepbar">
        {STEPS.map((s, i) => (
          <div key={t(s)} className="flex flex-1 items-center gap-2">
            <span
              className={`flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-[10px] font-bold ${
                i < step ? 'bg-emerald-600 text-ink' : i === step ? 'bg-accent text-ink' : 'border border-line text-ink-dim'
              }`}
            >
              {i < step ? '✓' : i + 1}
            </span>
            <span className={`text-xs ${i === step ? 'text-ink' : 'text-ink-dim'}`}>{t(s)}</span>
            {i < STEPS.length - 1 && <span className="h-px flex-1 bg-line" />}
          </div>
        ))}
      </div>

      {/* 体 */}
      <div className="mt-4 min-h-[180px]" data-testid={`wizard-step-${step + 1}`}>
          {step === 0 && (
            <div className="space-y-3">
              <label className="block text-sm" htmlFor="ob-dir">
                {t('图片目录（服务器可访问的绝对路径）')}
              </label>
              <div className="flex gap-2">
                <input
                  id="ob-dir"
                  value={dir}
                  onChange={(e) => {
                    setDir(e.target.value)
                    setScan(null)
                    setScanErr(null)
                  }}
                  placeholder={t('例如 /path/to/photos 或 D:\\Pictures')}
                  className="w-full rounded-md border border-line bg-panel-2 px-3 py-2 text-sm outline-none focus:border-accent"
                />
                {desktop && (
                  <Button
                    type="button"
                    disabled={picking || scanning}
                    onClick={doPick}
                    title={t('打开系统文件夹选择框（桌面版）')}
                    variant="secondary"
                    size="md"
                  >
                    {picking ? '选择中…' : '选择文件夹…'}
                  </Button>
                )}
                <Button
                  type="button"
                  disabled={!dirValid || scanning}
                  onClick={doScan}
                  variant="secondary"
                  size="md"
                >
                  {scanning ? '扫描中…' : '扫描'}
                </Button>
              </div>
              {scan && (
                <div className="rounded border border-emerald-700/40 bg-emerald-950/20 px-3 py-2 text-xs leading-5 text-emerald-300">
                  {t('扫描完成：{n} 张图片 · 共 {size}', { n: scan.imageFiles, size: formatSize(scan.totalBytes) })}
                  <span className="mt-1 block truncate text-ink-dim" title={scan.preview.join('\n')}>
                    {scan.preview.slice(0, 3).map((p) => `…${p.slice(-40)}`).join(' / ')}
                    {scan.preview.length > 3 ? t(' 等') : ''}
                  </span>
                </div>
              )}
              {scanErr && (
                <p className="rounded border border-red-600/40 bg-red-950/30 px-3 py-1.5 text-xs text-red-300">
                  {scanErr}
                </p>
              )}
              <p className="text-xs leading-5 text-ink-dim">
                {desktop
                  ? t('桌面版可直接「选择文件夹…」；路径由本机服务扫描，不复制文件（原地索引）或复制入库由下一步决定。哈希重复的图片会在导入时自动去重。')
                  : t('v1 为单用户自托管模式（R4），目录指 damdesk 服务所在机器上的路径。哈希重复的图片会在导入时自动去重。')}
              </p>
            </div>
          )}
          {step === 1 && (
            <div className="space-y-3">
              <p className="text-sm">{t('图片文件如何进入资产库？')}</p>
              {(
                [
                  ['inplace', t('原地索引（推荐）'), t('不复制文件，直接索引原目录。省磁盘、删除外部文件会同步失效。')],
                  ['copyin', t('复制入库'), t('复制到 DAM 数据目录统一管理，源目录可随意移动。占用双倍磁盘。')],
                ] as const
              ).map(([v, title, desc]) => (
                <button
                  key={v}
                  type="button"
                  onClick={() => setMode(v)}
                  className={`block w-full rounded-md border px-4 py-3 text-left ${mode === v ? 'border-accent bg-accent-soft' : 'border-line bg-panel-2 hover:border-ink-dim'}`}
                >
                  <p className="text-sm font-medium">{title}</p>
                  <p className="mt-0.5 text-xs leading-5 text-ink-dim">{desc}</p>
                </button>
              ))}
            </div>
          )}
          {step === 2 && (
            <div className="space-y-4">
              <label className="flex cursor-pointer items-start gap-3">
                <input
                  type="checkbox"
                  checked={autoTag}
                  onChange={(e) => setAutoTag(e.target.checked)}
                  className="mt-0.5 h-4 w-4 accent-[#4f8cff]"
                />
                <span>
                  <span className="block text-sm">{t('导入后自动打标')}</span>
                  <span className="mt-0.5 block text-xs leading-5 text-ink-dim">
                    {t('导入完成后自动把新图加入打标队列并调用当前 tag provider（设置页可换：dam-ai / dashscope / zhipu / Ollama / 自定义）生成标签提案；到「打标确认」页逐张确认后才会写入图库。provider 未配置或被占用时任务保持待跑，可稍后在打标任务页启动。')}
                  </span>
                </span>
              </label>
            </div>
          )}
      </div>
    </Dialog>
  )
}

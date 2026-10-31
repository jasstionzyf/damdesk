/**
 * PROC-RES-3（KI-40，卡 task）：处理产物**应用内预览**。
 *
 * 为什么不是 `<Lightbox>`：Lightbox 绑的是图库资产（`items: AssetView[]` + `thumbUrl`/`originalUrl`
 * + 翻页键位），产物不是资产 —— 没有 assetId、路径在 `{data_dir}/compliance/fixes/<job>/…`，
 * 塞进去只能伪造 assetId（缩略图 URL 404、翻页键位指向错误集合）。所以复用**同一个浮层原语**
 * `lib/modal` 的 `<Dialog>`：Esc 栈、点遮罩关闭、焦点归还、背景 inert 全都现成，只换内容区。
 *
 * 图片 URL 走同源 `/api/compliance/artifact?path=…`（`artifactUrl`）—— 容器形态与桌面形态同一个地址，
 * 不引入 Tauri 插件、不开新窗口、不依赖系统下载器。
 */
import { Button } from './Button'
import { useI18n } from '../i18n'
import { PathLink } from './PathLink'
import { Dialog } from '../lib/modal'
import { artifactUrl } from '../lib/api'
import { useArtifactDownload } from '../lib/artifactActions'

export interface ArtifactPreviewProps {
  /** 产物路径；null/空 = 关闭 */
  path: string | null
  /** 展示用文件名（缺省取路径 basename） */
  name?: string | null
  onClose: () => void
  testid?: string
}

function basename(p: string): string {
  const i = Math.max(p.lastIndexOf('/'), p.lastIndexOf('\\'))
  return i >= 0 ? p.slice(i + 1) : p
}

export function ArtifactPreview({ path, name, onClose, testid = 'artifact-preview' }: ArtifactPreviewProps) {
  const { t } = useI18n()
  const onDownload = useArtifactDownload()
  const p = (path ?? '').trim()
  const label = (name ?? '').trim() || (p ? basename(p) || p : t('产物'))

  return (
    <Dialog
      open={p !== ''}
      title={label}
      subtitle={t("处理产物预览（同源读取，不进图库、不改资产工作版本）")}
      onClose={onClose}
      testId={testid}
      widthClass="max-w-4xl"
      footer={
        <span className="flex items-center gap-2">
          <a
            href={artifactUrl(p, true)}
            onClick={(e) => onDownload(e, p, label)}
            className="inline-flex h-8 items-center gap-1 rounded border border-line bg-panel-2 px-3 text-xs text-ink hover:border-accent hover:text-accent"
            data-testid={`${testid}-download`}
          >
            {t('下载')}
          </a>
          <Button type="button" onClick={onClose} data-testid={`${testid}-done`}>
            {t('关闭')}
          </Button>
        </span>
      }
    >
      <div className="flex flex-col gap-3">
        <img
          src={artifactUrl(p)}
          alt={label}
          className="mx-auto max-h-[62vh] w-auto max-w-full rounded border border-line object-contain"
          data-testid={`${testid}-image`}
        />
        {/* 磁盘路径的唯一渲染组件（KI-9/KI-10）：「在文件夹中显示」的形态门控与人话原因都在里面 */}
        <PathLink path={p} mode="reveal" />
      </div>
    </Dialog>
  )
}

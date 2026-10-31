/**
 * 「找相似的」split button（KI-14 / 卡 task）—— 唯一实现。
 *
 * 主键 = 用当前档位执行（actions.findSimilar()）；右侧 caret 打开菜单切档位
 * （两项，带一句人话说明），选档即写偏好 + 立即以新档执行一次（参数必须
 * 「点了有后果」——这正是 KI-14 的根因：模式开关点了没反馈）。
 *
 * 挂载处：Inspector（图库 / 搜索经 InspectorDock 共用）；档位来源 lib/similarMode。
 */
import { useEffect, useRef, useState } from 'react'
import { useI18n } from '../i18n'
import { Button } from './Button'
import { SIMILAR_MODES, similarModeOption, type SimilarMode } from '../lib/similarMode'

export function SimilarSplitButton({
  mode,
  onModeChange,
  onRun,
  busy,
}: {
  /** 当前档位（页面持有，来自 useSimilarMode） */
  mode: SimilarMode
  onModeChange: (m: SimilarMode) => void
  /** 执行；可带档位覆盖（菜单选档 = 切档后立即以新档执行） */
  onRun: (mode?: SimilarMode) => void
  busy?: boolean
}) {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLSpanElement | null>(null)

  // 点外部 / Esc 关菜单（点菜单项自身不冒泡到 window，先于这里生效）
  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (ref.current && e.target instanceof Node && ref.current.contains(e.target)) return
      setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    window.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      window.removeEventListener('keydown', onKey)
    }
  }, [open])

  const cur = similarModeOption(mode)
  return (
    <span ref={ref} className="relative block w-full" data-testid="insp-find-similar-group">
      <span className="flex w-full">
        <Button
          size="md"
          className="flex-1 rounded-r-none"
          data-testid="insp-find-similar"
          disabled={busy}
          title={t('以这张图为查询：{label}（{desc}）。箭头可切档位', { label: t(cur.label), desc: t(cur.desc) })}
          onClick={() => onRun()}
        >
          {busy ? t('搜索中…') : t('找相似的 · {mode}', { mode: t(cur.label) })}
        </Button>
        <Button
          size="md"
          className="rounded-l-none border-l border-black/20 px-2"
          data-testid="insp-find-similar-caret"
          aria-haspopup="menu"
          aria-expanded={open}
          aria-label={t('选择找相似档位（语义相似 / 精准匹配）')}
          title={t('切档位：语义相似（同类的其它图）/ 精准匹配（同一张图的不同版本）')}
          onClick={() => setOpen((o) => !o)}
        >
          ▾
        </Button>
      </span>
      {open && (
        <span
          role="menu"
          aria-label={t('找相似档位')}
          data-testid="insp-find-similar-menu"
          className="absolute right-0 top-full z-30 mt-1 block w-72 rounded-md border border-line bg-panel p-1 text-left shadow-xl"
        >
          {SIMILAR_MODES.map((opt) => {
            const active = opt.value === mode
            return (
              <button
                key={opt.value}
                type="button"
                role="menuitemradio"
                aria-checked={active}
                data-testid={`similar-option-${opt.value}`}
                title={opt.title}
                onClick={() => {
                  setOpen(false)
                  onModeChange(opt.value)
                  // 选档即执行：参数选择必须立刻有可理解的后果（KI-14 根因）
                  onRun(opt.value)
                }}
                className={`block w-full rounded px-2 py-1.5 text-left text-xs ${
                  active ? 'bg-accent-soft text-ink' : 'text-ink hover:bg-panel-2'
                }`}
              >
                <span className="flex items-center gap-1.5">
                  {active && <span className="text-accent">✓</span>}
                  <span className="font-medium">{opt.label}</span>
                </span>
                <span className="mt-0.5 block text-[11px] leading-4 text-ink-dim">{opt.desc}</span>
              </button>
            )
          })}
        </span>
      )}
    </span>
  )
}

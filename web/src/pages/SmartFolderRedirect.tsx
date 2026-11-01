import { useEffect, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import { Button } from '../components/Button'
import { EmptyState } from '../components/StateViews'
import { api } from '../lib/api'
import { smartFolderHref } from '../lib/galleryUrlState'
import { parseQuery } from '../lib/smartFolders'

/**
 * KI-16 #7（卡 t_f6d214fb）：`/smart/:id` 只做**重定向**，不再是一张浏览面。
 *
 * 智能文件夹本质是「保存的筛选条件」，与图库筛选语义重复；独立页面会带来第三套网格 /
 * 检查器会话键 / 分页口径（R45）。现在统一收敛成：
 *   `/smart/:id` → `/?<保存的条件>&sf=<id>`（replace，不制造历史垃圾）
 * 图库按条件过滤并在筛选横幅上显示「智能文件夹：<名字> ×」。
 *
 * 老深链不 404：文件夹还在就跳图库；已被删除就给「文件夹不存在」可读提示（可回图库）。
 */
export function SmartFolderRedirect() {
  const { id } = useParams()
  const navigate = useNavigate()
  const [err, setErr] = useState<string | null>(null)

  useEffect(() => {
    const folderId = Number(id)
    if (!Number.isFinite(folderId) || folderId <= 0) {
      setErr(`无效的智能文件夹 id：${id}`)
      return
    }
    let stop = false
    api
      .smartFolder(folderId)
      .then((f) => {
        if (stop) return
        navigate(smartFolderHref(f.id, parseQuery(f.query)), { replace: true })
      })
      .catch((e) => {
        if (stop) return
        setErr(e instanceof Error ? e.message : String(e))
      })
    return () => {
      stop = true
    }
  }, [id, navigate])

  if (err) {
    return (
      <div className="flex min-h-0 flex-1 flex-col" data-testid="smart-redirect-error">
        <EmptyState
          icon="◎"
          title="智能文件夹不存在"
          desc={`${err}（可能已被删除或重命名）`}
          actions={
            <Button type="button" onClick={() => navigate('/', { replace: true })} variant="primary" size="md">
              返回图库
            </Button>
          }
          hint="智能文件夹现在只是图库的一组筛选条件，可在图库侧栏重新创建"
        />
      </div>
    )
  }

  return (
    <div data-testid="smart-redirect-pending" className="flex flex-1 items-center justify-center text-sm text-ink-dim">
      正在打开智能文件夹…
    </div>
  )
}

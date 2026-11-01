/**
 * 规格的「默认选中」—— **界面偏好**，不是规格属性。
 *
 * 为什么只在界面记：规格表（`plugin_spec`，migration 0028）没有 `isDefault` 字段，
 * 后端也还没有「默认规格」语义。用户要的是「同一个插件下有多条规格时，检查/处理入口
 * 默认选中哪一条」——这是**我这台机器上的操作习惯**，不是规格的标准内容（换台机器不该
 * 跟着变，同一规格被分享出去也不该带走别人的默认）。
 *
 * 一旦后端补 `isDefault`（或 settings 里加 `pluginDefaultSpec`），这里换成读接口即可，
 * 上层（插件详情页 / 试跑入口）不用改：它们只调 `getDefaultSpecId` / `setDefaultSpecId`。
 */
const KEY = 'dam.pluginDefaultSpec'

function readAll(): Record<string, number> {
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return {}
    const parsed = JSON.parse(raw) as unknown
    if (!parsed || typeof parsed !== 'object') return {}
    const out: Record<string, number> = {}
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof v === 'number' && Number.isFinite(v)) out[k] = v
    }
    return out
  } catch {
    return {}
  }
}

/** 该插件下用户选的默认规格 id（没选过 / 已删 → null） */
export function getDefaultSpecId(pluginId: string): number | null {
  return readAll()[pluginId] ?? null
}

/** 设为默认（`null` = 清除） */
export function setDefaultSpecId(pluginId: string, specId: number | null): void {
  const all = readAll()
  if (specId == null) delete all[pluginId]
  else all[pluginId] = specId
  try {
    localStorage.setItem(KEY, JSON.stringify(all))
  } catch {
    /* 隐私模式 / 配额满：默认规格只是便利，存不下不算故障 */
  }
}

/**
 * 从候选里挑「这次该用哪条规格」：
 * 1. 用户设的默认（且仍在列表里、且启用）→ 用它；
 * 2. 否则第一条**启用**的规格（只有一条规格时这就是它，用户不用做选择）；
 * 3. 都没有 → null（调用方提示「先新建规格」）。
 */
/** 规格是否启用：`/api/specs` 落库是 0|1，老界面代码写的是 boolean —— 两头都认，只判「明确停用」 */
export function specEnabled(v: boolean | number | null | undefined): boolean {
  return v !== false && v !== 0 && v != null
}

export function pickDefaultSpecId<
  T extends { specId: number; enabled: boolean | number },
>(pluginId: string, specs: T[]): number | null {
  const chosen = getDefaultSpecId(pluginId)
  if (chosen != null) {
    const hit = specs.find((s) => s.specId === chosen && specEnabled(s.enabled))
    if (hit) return hit.specId
  }
  return specs.find((s) => specEnabled(s.enabled))?.specId ?? null
}

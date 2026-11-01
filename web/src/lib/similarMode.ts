/**
 * 找相似「档位」（KI-14 / 卡 t_a0350b71）—— 唯一实现，图库 / 搜索 / 命令面板共用。
 *
 * 口径（KI-14 附带裁决）：模式是**参数**，不得以独立开关出现在网格工具条。
 * 档位选择只住在两个合法位置：
 *  1. Inspector「找相似的」split button 的下拉菜单（components/SimilarSplitButton）；
 *  2. 搜索页以图搜图入口旁的同一组选择器（与菜单文案逐字一致）。
 *
 * 存储：档位写本地偏好（localStorage `dam.similarMode`），**不写 URL**；
 * 旧深链 `?simMode=visual_precise` 仍兼容——挂载时读到即生效（只读，不回写）。
 */
import { useCallback, useState } from 'react'
import { tGlobal } from '../i18n'

export type SimilarMode = 'semantic' | 'visual_precise'

export interface SimilarModeOption {
  value: SimilarMode
  /** 档位名（两处入口文案必须逐字一致；中文原文键，展示经 similarModeOption() 取词） */
  label: string
  /** 一句人话说明（菜单副文案） */
  desc: string
  /** 悬浮提示（沿用 KI-5 的完整口径） */
  title: string
}

/** 原文键常量（i18n-D：模块级不固化译文；展示走 similarModeOption() 的 tGlobal 取词） */
export const SIMILAR_MODES: readonly SimilarModeOption[] = [
  {
    value: 'semantic',
    label: '语义相似',
    desc: '同类的其它图',
    title: '语义相近的图（默认，按模型标定的相似度下限过滤）',
  },
  {
    value: 'visual_precise',
    label: '精准匹配',
    desc: '同一张图的不同版本',
    title: '只要极像的图：近似重复 ≥98% / 同图变体 ≥90%（裁剪、换色、加字、压缩重存）',
  },
] as const

/** i18n-D：取词唯一入口（label/desc/title 按当前 locale 翻译） */
export function similarModeOption(m: SimilarMode): SimilarModeOption {
  const hit = SIMILAR_MODES.find((o) => o.value === m) ?? SIMILAR_MODES[0]
  return {
    value: hit.value,
    label: tGlobal(hit.label),
    desc: tGlobal(hit.desc),
    title: tGlobal(hit.title),
  }
}

export function parseSimilarMode(v: unknown): SimilarMode | null {
  return v === 'semantic' || v === 'visual_precise' ? v : null
}

const PREF_KEY = 'dam.similarMode'

export function readSimilarModePref(): SimilarMode {
  try {
    return parseSimilarMode(localStorage.getItem(PREF_KEY)) ?? 'semantic'
  } catch {
    return 'semantic'
  }
}

export function writeSimilarModePref(m: SimilarMode) {
  try {
    localStorage.setItem(PREF_KEY, m)
  } catch {
    /* 存储不可用：档位只在本次会话内生效 */
  }
}

/** 挂载时的初始档位：URL 深链（`?simMode=`，兼容旧链接，只读）优先，其次偏好，默认语义。 */
export function initialSimilarMode(): SimilarMode {
  try {
    const sp = new URLSearchParams(window.location.search)
    if (sp.has('simMode')) return parseSimilarMode(sp.get('simMode')) ?? 'semantic'
  } catch {
    /* SSR / 无 window：走偏好 */
  }
  return readSimilarModePref()
}

/**
 * 找相似档位状态（页面级）：每页一份（图库 / 搜索各自挂载互不干扰）。
 * setMode 同时落偏好；不写 URL（KI-14：参数不进工具条，也不制造历史）。
 */
export function useSimilarMode(): { mode: SimilarMode; setMode: (m: SimilarMode) => void } {
  const [mode, setM] = useState<SimilarMode>(initialSimilarMode)
  const setMode = useCallback((m: SimilarMode) => {
    writeSimilarModePref(m)
    setM(m)
  }, [])
  return { mode, setMode }
}

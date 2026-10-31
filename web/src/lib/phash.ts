/**
 * E5 去重对比视图：pHash 相似度计算（纯前端，数据来自 /api/dedup/groups 的
 * member.pHash hex 字符串）。64-bit pHash：汉明距离 d → 相似度 (64-d)/64。
 */

export function phashBits(hex: string | null | undefined): bigint | null {
  if (!hex || !/^[0-9a-fA-F]{1,16}$/.test(hex)) return null
  return BigInt('0x' + hex)
}

/** 汉明距离（任一 hash 非法 → null） */
export function phashHamming(a: string | null | undefined, b: string | null | undefined): number | null {
  const ba = phashBits(a)
  const bb = phashBits(b)
  if (ba === null || bb === null) return null
  let x = ba ^ bb
  let d = 0
  while (x) {
    d += Number(x & 1n)
    x >>= 1n
  }
  return d
}

/** 相似度百分比（0-100 整数；无法计算 → null） */
export function phashSimilarityPct(
  a: string | null | undefined,
  b: string | null | undefined,
): number | null {
  const d = phashHamming(a, b)
  if (d === null) return null
  return Math.round(((64 - d) / 64) * 100)
}

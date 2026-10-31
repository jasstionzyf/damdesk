/**
 * KI35-P3：选择集上的能力入口「送去处理 →」—— 打开**能力面板**（全局 Dialog
 * `?dialog=process&ids=…`），图库与搜索结果页共用同一份实现。
 *
 * 为什么在这里而不在各页自拼按钮：入口收敛的判据是「用户看到的是同一句话」——
 * 两个页面各写一遍就会出现「送去处理」和「开始处理」两种说法（KI-16 #5 的老毛病）。
 * 旧的「检查 / 处理」两个动词按钮随 P1 撤掉主导航 tab 后在此收口：检查、处理
 * 都是能力（由 manifest kinds 区分），入口只剩这一个。
 *
 * 行为：只负责把选择集（与可选的「当前筛选结果」上下文）带进面板 —— 选能力、选方案、
 * 跑，都在面板里完成（一个能力一个入口）；本组件**不发请求、不改数据**。
 */
import { useOverlay } from '../lib/modal'
import { stashProcessFilterScope, type ProcessFilterScope } from '../lib/sendToProcess'

import { Button } from './Button'
import { useI18n } from '../i18n'

export function VerbActions({
  ids,
  filterScope,
}: {
  ids: number[]
  /**
   * 打开面板时随手寄存「当前筛选结果」上下文（与导出 Dialog 同一套口径：browse 条件过滤
   * 才有；文搜图/相似结果是排序结果不是条件 → 传 undefined，面板少一个范围选项，不虚造）。
   */
  filterScope?: () => ProcessFilterScope | null
}) {
  const { t } = useI18n()
  const ov = useOverlay('dialog', 'process', ['ids'])
  const none = ids.length === 0
  return (
    <Button
      type="button"
      data-testid="verb-send"
      disabled={none}
      title={
        none
          ? t('先选中要处理的图片')
          : t('为选中的 {n} 张选一个能力：检查能不能用，或变成想要的样子', { n: ids.length })
      }
      onClick={() => {
        stashProcessFilterScope(filterScope?.() ?? null)
        ov.openOverlay({ ids: ids.join(',') })
      }}
    >
      {t('送去处理 →')}
    </Button>
  )
}

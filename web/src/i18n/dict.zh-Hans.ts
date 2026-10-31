/**
 * 简体中文字典（i18n-A 基座，卡 task）。
 *
 * 形态：**source-text-as-key** —— key 就是界面中文原文，中文环境下查不中时
 * `t()` 原样返回 key，所以这里默认**不需要**任何条目（薄壳）。
 * 只为「带占位符的模板 / ctx 消歧」留条目：这两类不能靠原文兜底。
 * B/C/D 批迁移时只往 en-US 灌词条，本文件保持薄。
 */
export default {} as Record<string, string>

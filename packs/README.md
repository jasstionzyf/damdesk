# packs/plugins — 官方行业包

五个随产品分发的官方包：`ecommerce`（电商主图合规）/ `realestate`（房产房源图合规）/
`imaging`（图片处理）/ `segment`（抠图换背景）/ `upscale`（超分）。

协议与字段权威：`docs/industry-pack-protocol.md`（`api_version` 恒 `"1.1"`）。

## 多语言（i18n-F，2026-10-26）

用户面展示文案（manifest 的 `name` / `short_name` / `summary`、rules.json 的 `rules[].name`、
`result_columns[].label`）同时接受两种形态（协议 §4.0.1，不升 `api_version`）：

- 字符串 `"合规检查"` = 全语言通用（老形态，照常工作）；
- 对象 `{"zh-Hans": "合规检查", "en-US": "Compliance Check"}` = 按语言给值。

宿主取值链：请求语言 → `zh-Hans` → 任意一条 → 空串（永不崩）。

**翻译口径**：官方五包全部提供 zh-Hans + en-US 两份。**第三方/私有包没写英文 → 英文界面回退显示中文**，
这是可接受边界，不是错误（宿主只会 `eprintln` 提醒一次）；想补英文就照上面对象形态加键，装新版即生效。
注意：en 只进展示层 —— 插件代码里的默认名（如 `ruleset.RULES[].name` / `PACK_INFO["name"]`）
保持中文单语，宿主展示读 manifest/rules 资源，不读代码默认值。

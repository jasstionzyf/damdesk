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

## 发布与版本号（铁律：同版本不许换内容）

宿主按协议 §7 铁律 7 拒绝「同 `id@version` 换内容」，错误码 `pack_version_conflict`，
判等口径是 **manifest.json 原文 sha256**（`crates/pack-host/src/manifest.rs::PackIdentity::plugin_hash`）。
内置包（`pack_builtin_protected`）**不可卸载**，所以一旦发布了「同号新内容」的包，
**装过旧版本的机器永远升不上去**（真机实锤）。

因此本目录多一份台账 `packs/pack-content-lock.json`（发布史：`id@version → 包内容摘要`）：

- 内容摘要 = 归档里除 `manifest.json` 外的全部文件；`plugin/__init__.py` 的 `PACK_ID` / `PACK_VERSION`
  两行算「包身份」（构建器按 id/version 改写），计算前抹掉；
- 出包前硬校验（`scripts/pack_release_guard.py`）：内容变了但版本号是已发布过的 → **出包直接失败**；
  内容没变 → 同版本重导幂等放行；新版本号必须严格大于已发布的最高版本；
- 交付出包命令（过校验 + 入账）：
  ```bash
  python3 scripts/k15_build_packs.py --release-version <V> \
      --out ~/deliver/dampacks-<短sha>/com.soujpg.pack.ecommerce-<V>.dampack --record
  ```
  `python3 scripts/k15_build_packs.py --out <dir>`（不带 `--release-version`）是**夹具模式**，
  只用于 E2E 装/切/回滚演练，产物不可交付（落 `deliver/dampacks*` 会被拒）。
- **改了包里任何文件（判定逻辑/规则/资源/params schema）→ 必须同时升 `manifest.json` 的 `version`
  与 `plugin/__init__.py` 的 `PACK_VERSION`**；已在装机器的升级姿势 = 新版本号 + activate。
- 回归门：`bash scripts/pack_release_guard_selftest.sh`（已挂进 `test_gate.sh` L0）。

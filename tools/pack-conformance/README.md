# pack-conformance — 行业包协议一致性套件

协议权威文档：`docs/industry-pack-protocol.md`。本目录是它的**可执行规格**：
假网关 + 真实子进程 + 真实 stdio 管道，16 条断言（01–12 协议面 + 13/14 i18n + 15/16 旧契约拒装指路），任何语言的包都必须过。

```bash
# 验一个包（16/16 PASS 才算过，退出码 0）
python3 tools/pack-conformance/run.py --plugin tools/pack-conformance/fixtures/good
python3 tools/pack-conformance/run.py --plugin /path/to/pack --json-out /tmp/r.json
python3 tools/pack-conformance/run.py --plugin /path/to/pack --only 02,07   # 只跑部分断言

# 验套件自身：5 官方包 + good 必须 16/16，四个违规变体必须命中各自的预期断言集合
bash tools/pack-conformance/run_all.sh
```

## 文件

| 文件 | 作用 |
|---|---|
| `run.py` | 宿主侧 harness：拉起插件、行式 JSON-RPC 客户端、16 条断言、超时铁律（cancel→kill） |
| `gateway.py` | 假网关：按 `manifest.capabilities` 放行 13 个端点，回固定桩数据并记录命中/403 |
| `protocol.py` | 协议常量与 manifest / Verdict 校验（宿主实现与各语言 SDK 的对照基准） |
| `pack_lint.py` | manifest 静态门禁：强制显式 `outputs`、api_version=2.0、旧键 `kinds` 拒装（缺 → 非零退出） |
| `run_all.sh` | 套件自检：逐变体比对失败断言集合 |
| `fixtures/good` | Python 参考实现（正确的包长什么样；**字符串形态**名 —— 老包回归保护） |
| `good-i18n`（`run_all.sh` 运行时派生） | good 的对象形态名副本（`run.py` 上多语言字段 §4.0.1 的端到端证明） |
| `fixtures/bad-*` | 四个违规变体（stdout 污染 / 缺方法 / 错误码乱用 / health 不应答） |

## 规则集泛化（K04 起）

断言不再写死参考实现的 rule id（`A2_min_short_side` / `A4_subject_ratio` / `A5_overlay_text` /
`A6_category`）——否则只有 `fixtures/good` 及其变体能跑，任何语义不同名的真包（官方电商包是
`R0..R5`）都会因为「未知规则 → skipped」在 06/07/09/10/11/12 上被动红。

现在的规则集来源优先级：

1. `manifest.json` 的可选扩展字段 `rules: [{"id": ...}]`（宿主 `pack-host` 的 manifest 反序列化是
   `deny_unknown_fields`，真包**不要**写这个字段，会启动失败）；
2. 启动时探测 `pack.info` 的 `rules[]`（真包走这条）；
3. 都拿不到才回落参考实现的 `A*` 名（`run_all.sh` 四个变体的历史行为不变）。

配套调整（协议面断言一条没松）：

- `07`：请求**全部**规则，判据改成「网关确实收到 `/vl/judge`、且确有 verdict 落 `undetermined`
  并写了 `note`」——不再点名某条 VL 规则；
- `08/09/10/12`：一律 `rules: null`（全部规则），谁先撞上能力缺失/取消位都算数；
- `11`：`repair` 也传 `rules: null`，让包按自己的修复策略挑可修规则（不再假定它一定修 A2/A4）。

## 边界（别误读结果）

- 假网关**不跑** u2net/OCR/VL，只按资产文件名约定回固定值 → 这里过了**不代表算法对**；
  算法质量由 K00 基线、K07 等价门禁、K11 一致性卡负责。
- 这里管的是协议面：传输、方法、数据结构、错误码、取消、能力白名单、产物落盘、job 归账。
- 桩语义（文件名约定：`_1600x1600`、`fg0.30`、`overlay`、`vlundet`、`slow`）见 `gateway.py` 文件头。

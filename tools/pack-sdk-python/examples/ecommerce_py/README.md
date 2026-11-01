# 电商样例包（Python SDK 版）

用 `dampack-sdk` 写的电商主图合规包，**同时是 SDK 的写法范本与一致性对账对象**。

```
manifest.json           # 协议元数据（宿主按它启动；不许加 rules 字段，pack-host 是 deny_unknown_fields）
plugin/__init__.py      # 编排：judge / repair（协议壳全交给 PackServer）
plugin/rules.py         # 六条规则的纯判定函数 + 阈值（与官方包 v1.0.0 逐条对齐）
plugin/geometry.py      # 几何闭式解 crop → letterbox → upscale（移植自官方包）
plugin/maskpng.py       # 纯标准库 PNG 掩码（给 /op/inpaint）
resources/rules.json    # 规则/阈值（与 rules.THRESHOLDS 同源，仅供人工核对）
requirements.txt        # 唯一依赖：dampack-sdk
```

## 跑

```bash
# 协议一致性（12 条断言，假网关 + 真子进程）
python3 tools/pack-conformance/run.py --plugin tools/pack-sdk-python/examples/ecommerce_py

# 与旧引擎 10 张基线对账
python3 tools/pack-sdk-python/scripts/consistency_10.py
```

仓内直跑时插件会按仓库布局自己找到 SDK（`DAMPACK_SDK_PATH` 可覆盖）；装成包之后由包内 venv 提供。

## 与官方包 `packs/plugins/ecommerce` 的差异

| 维度 | 官方包 | 本样例包 |
|---|---|---|
| 协议壳 | 自己实现 dispatch / cancel / report 账本 | `PackServer` 全接管 |
| 规则语义 | 基准（R0..R5、短路、修复策略、阈值） | 逐条对齐（阈值下限 `min` 由 `options.thresholds` 可覆盖，便于与旧引擎 0.65 口径对账） |
| 度量精度 | `score` 截断到 3 位小数 | **不截断**（逐值 1e-6 对账需要；官方包截断属展示口径） |
| 依赖 | 无第三方库 | 仅 SDK（SDK 也零第三方依赖） |

两者的判定口径一致性由 `scripts/consistency_10.py` 对着 K00 旧引擎基线核：
确定性规则 R1/R2/R4 的 status 与度量值逐值一致，`score` 按包内定义投影后一致（容差 1e-6）。

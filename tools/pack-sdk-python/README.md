# pack-sdk-python — DAMDesk 行业包 Python SDK（官方主路径）

协议权威：`docs/industry-pack-protocol.md` v1.0。可执行规格：`tools/pack-conformance/`（12 条断言）。
本目录是**协议面的 Python 实现**：第三方和官方包都该基于它写，底下的 JSON-RPC / 取消 / 账本 /
stdout 保护不再由每个包各写一遍。

```bash
# 一条命令自检（单测 → conformance 12/12 → 每包 venv → 10 张一致性）
bash tools/pack-sdk-python/run_tests.sh

# 单独验一个包（任何语言的包都是这条）
python3 tools/pack-conformance/run.py --plugin <pack_dir>
```

## 1. 目录

| 路径 | 作用 |
|---|---|
| `dampack/protocol.py` | 协议常量 + manifest / Verdict / pack.info 校验（与 conformance 的常量逐字同义，单测钉住） |
| `dampack/errors.py` | `RpcError` + HTTP→错误码映射表（403=能力缺失 / 404=资产不可读 / 400=参数非法） |
| `dampack/verdict.py` | Verdict 七字段构造器（`undetermined` 必须带 note） |
| `dampack/gateway.py` | 网关客户端：13 个端点 + 回环免代理 + 错误映射 + `max_tokens≤4096` |
| `dampack/server.py` | `PackServer`：装饰器注册、stdout 保护、取消、`report` 账本、兜底错误帧 |
| `dampack/launcher.py` + `dampack_launcher.py` | `python -m dampack_launcher <pack_dir>`（与宿主 K02 启动器同契约） |
| `dampack/venv.py` | 每包 venv 创建/装依赖（裁决 2026-10-20：不用 vendor+sys.path 覆盖） |
| `tests/` | 59 例单测（真实子进程 + 真实 socket；含与 conformance 常量/宿主启动器的对照断言） |
| `examples/ecommerce_py/` | 样例电商包（SDK 写法范本 + 一致性对账对象） |
| `scripts/consistency_10.py` | 10 张一致性对账（vs K00 旧引擎基线） |

## 2. 最小包（四件事都不用自己写）

```python
# plugin/__init__.py
from dampack import PackServer, Gateway, verdict

pack = PackServer(pack_id="com.example.pack", name="示例包", version="1.0.0",
                  rules=[{"id": "X1"}], capabilities=["io.read", "vl.judge"],
                  deterministic=False, timeout_secs=60)

@pack.method("judge")
def judge(params):
    path = params["asset"]["path"]
    meta = Gateway.from_env().io_read(path)          # 能力出口只有一个
    passed = meta["width"] >= 1000
    return {"asset_id": params["asset"]["id"],
            "verdicts": [verdict("X1", "pass" if passed else "fail",
                                 score=float(meta["width"]), label="min_width")]}

def serve():
    return pack.serve()

if __name__ == "__main__":
    serve()
```

`PackServer` 自动提供 `pack.info`（由元数据生成，和 manifest 不漂移）、`health`、`cancel`、
`report`（按 `emit()` 的账本汇总）。`judge/repair` 的应答会被校验：`verdicts` 缺失或字段不合法
直接回 `-32000`，不会把半截结果交给宿主。

## 3. 铁律（SDK 帮你守，但要知道为什么）

1. **stdout 只走协议帧**。`serve()` 一接手就把真 fd1 存下来、fd1 接到 stderr；
   之后任何库里/插件里的 `print` 都落 stderr。帧 JSON 用 `ensure_ascii=False`、逐行 flush、
   Windows `newline="\n"`。
2. **403 不许吞**。网关 403 → `ERR_CAPABILITY_MISSING(-32001)`，不许降级成 pass/fail；
   401 是宿主配置问题（`-32000`），404 是资产不可读（`-32002`），400 是参数非法（`-32003`）。
3. **`undetermined` 不是 fail**。判定不了（VL 解析失败、模型缺席）就落 `undetermined` 并写 note，
   构造器会拒绝没有 note 的 `undetermined`。
4. **取消后不许再发结论**。`pack.check_cancel(job_id, stage)` 抛 `-32004`；
   `pack.emit()` 在取消后直接拒绝。
5. **每包独立 venv**。依赖写 `requirements.txt`；装的时候
   `DAMPACK_SDK_DIR=<SDK 源目录> <venv>/bin/pip install -r requirements.txt`。
   包内 venv 零第三方库时，装的是 SDK 自己（23 KB wheel，可断网装）。

## 4. 实测记录（供文档引用，2026-10-21 gpu7 宿主 python3.14）

| 指标 | 值 | 取法 |
|---|---|---|
| SDK 体积 | **157.8 KiB / 24 文件 / 3918 行**（含 examples、tests、scripts） | `scripts/consistency_10.py` 的 `sdk_size` 段 |
| SDK wheel | **23 KB**（`dampack_sdk-1.0.0-py3-none-any.whl`） | `run_tests.sh` ③ 段的 pip 输出 |
| 启动时间（spawn → health） | **34 ms**（10 次取中位数量级，回放网关） | `consistency_10.py --json-out` 的 `startup_ms` |
| conformance | **12/12 PASS（5.8s）** | `python3 tools/pack-conformance/run.py --plugin tools/pack-sdk-python/examples/ecommerce_py` |
| 单测 | **59 例全绿（8.8s）** | `python3 -m unittest discover -s tools/pack-sdk-python/tests` |
| 10 张一致性 | 确定性规则 **30/30 status + 30/30 度量 + 30/30 score**（容差 1e-6） | `consistency_10.py`（回放口径） |

## 5. 边界（别误读）

- 本 SDK 只管**协议面**。合规规则/阈值/修复策略属于各行业包，SDK 不内置任何业务语义。
- `consistency_10.py` 的网关输入是**回放**（K00 基线里的测量值），证明的是「同一批测量值进去，
  规则结论与派生值一致」；端到端（真 u2net / 真 VL / 真语料）由 K07 等价门禁负责。
- VL 规则天然非确定性：`--vl-live` 跑真 `deepseek-flash` 时 f002/f003 的 R0 会从 pass 翻 fail
  （见 `scripts/consistency_10.py --help` 与报告 §4），所以 VL 列只作参考、不作门禁。

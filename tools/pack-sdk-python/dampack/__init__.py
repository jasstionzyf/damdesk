"""DAMDesk 行业包 SDK（Python）—— 第三方与官方包共用的插件运行时。

只覆盖协议面（`docs/industry-pack-protocol.md` v1.0）：
    - 常量与校验：`dampack.protocol`
    - 错误码：`dampack.errors`
    - Verdict 构造：`dampack.verdict`
    - 宿主能力网关客户端：`dampack.gateway`
    - 行式 JSON-RPC 服务（含 stdout 保护/装饰器注册/取消）：`dampack.server`
    - 启动器（`python -m dampack.launcher <pack_dir>`）：`dampack.launcher`
    - 每包 venv 创建/装依赖：`dampack.venv`

零第三方依赖（只用标准库）：包内 venv 可在断网环境瞬间建好，
桌面安装包也不会因为 SDK 背上 numpy/Pillow 的体积与 CVE。

最小用法：

    from dampack import PackServer, Gateway, verdict

    pack = PackServer(pack_id="com.example.pack", name="示例包", version="1.0.0",
                      rules=[{"id": "X1"}], capabilities=["io.read"])

    @pack.method("run")
    def judge(params):
        info = Gateway.from_env().io_read(params["asset"]["path"])
        return {"asset_id": info["path"],
                "verdicts": [verdict("X1", "pass", score=1.0, label="ok")]}

    if __name__ == "__main__":
        pack.serve()
"""
from __future__ import annotations

from .errors import RpcError
from .errors import (
    ERR_ASSET_UNREADABLE,
    ERR_CANCELED,
    ERR_CAPABILITY_MISSING,
    ERR_INVALID_PARAMS,
    ERR_METHOD_NOT_FOUND,
    ERR_OTHER,
    ERR_PARSE,
    ERR_TIMEOUT,
    PLUGIN_ERROR_CODES,
)
from .gateway import Gateway
from .protocol import (
    API_VERSION,
    ARTIFACT_KINDS,
    GATEWAY_ENDPOINTS,
    OUTPUTS,
    KNOWN_CAPABILITIES,
    METHODS,
    NOTIFICATIONS,
    RESULT_STATUSES,
    ROW_FIELDS,
    STATUSES,
    VERDICT_FIELDS,
    VERDICT_STATUSES,
    validate_manifest,
    validate_pack_info,
    validate_result_row,
    validate_verdict,
)
from .server import PackServer, log, protect_stdout
from .verdict import failed, passed, skipped, undetermined, verdict
from .result import (
    export_file,
    failed_row,
    judge_row,
    process_row,
    reply,
    result_row,
    skipped_row,
    working_version,
)

__version__ = "1.0.0"

__all__ = [
    "API_VERSION",
    "ARTIFACT_KINDS",
    "ERR_ASSET_UNREADABLE",
    "ERR_CANCELED",
    "ERR_CAPABILITY_MISSING",
    "ERR_INVALID_PARAMS",
    "ERR_METHOD_NOT_FOUND",
    "ERR_OTHER",
    "ERR_PARSE",
    "ERR_TIMEOUT",
    "GATEWAY_ENDPOINTS",
    "Gateway",
    "OUTPUTS",
    "KNOWN_CAPABILITIES",
    "METHODS",
    "NOTIFICATIONS",
    "PLUGIN_ERROR_CODES",
    "PackServer",
    "RESULT_STATUSES",
    "ROW_FIELDS",
    "RpcError",
    "STATUSES",
    "VERDICT_FIELDS",
    "VERDICT_STATUSES",
    "__version__",
    "export_file",
    "failed",
    "failed_row",
    "judge_row",
    "log",
    "passed",
    "process_row",
    "protect_stdout",
    "reply",
    "result_row",
    "skipped",
    "skipped_row",
    "undetermined",
    "validate_manifest",
    "validate_pack_info",
    "validate_result_row",
    "validate_verdict",
    "verdict",
    "working_version",
]

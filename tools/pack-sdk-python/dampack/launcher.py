"""SDK 版启动器：`<解释器> -m dampack.launcher <pack_dir>`。

与宿主 `tools/dampack_launcher.py`（K02）**同契约**，差别只有一处：
本启动器在装配 `sys.path` 时**额外**把 SDK 自身所在目录插进去（第三方包把 SDK 装进
自己的 venv，或仓内样例按仓库布局直接跑），这样 `import dampack` 永远成立。
退出码、stdout 保护、`--probe` 行为与宿主版逐字一致（`tests/test_launcher.py` 有对照断言）。

为何包内需要启动器：
    宿主 spawn 的是 `python -m dampack_launcher <pack_dir>`；
    包内 venv 只装自己的依赖时，宿主那份启动器在宿主目录、靠 PYTHONPATH 注入；
    包自带一份（pip 装 SDK 后 site-packages 里就有 `dampack_launcher`）就能在
    没有宿主 PYTHONPATH 的场景（诊断、`python -X importtime` 排障、第三方自测）照样跑。

铁律：stdout 只允许协议帧 / probe 的 JSON 行。启动器**先**把 fd1 存下来再把 fd1 接到
stderr，装配期/import 期任何 `print` 都落 stderr；交给插件运行前把真 stdout 还回去
（插件侧的 `PackServer.serve()` 会再做一次同样的保护）。
"""
from __future__ import annotations

import importlib
import json
import os
import sys
import traceback
from typing import NoReturn

EXIT_OK = 0
EXIT_USAGE = 2
EXIT_LOAD = 3

SDK_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _protect_stdout():
    real_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    return os.fdopen(real_fd, "w", encoding="utf-8", newline="\n")


_raw_out = None      # 首次调用时初始化（import 本模块不动 fd，方便单测）
_real_stdout_fd = None


def _ensure_guard():
    global _raw_out, _real_stdout_fd
    if _raw_out is None:
        _real_stdout_fd = os.dup(1)
        os.dup2(2, 1)
        sys.stdout = sys.stderr
        _raw_out = os.fdopen(os.dup(_real_stdout_fd), "w", encoding="utf-8", newline="\n")
    return _raw_out


def hand_over_stdout():
    """把真 stdout 还给插件进程（插件自己再 dup 一次做协议保护）。"""
    _ensure_guard()
    os.dup2(_real_stdout_fd, 1)
    sys.stdout = os.fdopen(os.dup(1), "w", encoding="utf-8", newline="\n")


def _fail(exit_code: int, reason: str) -> NoReturn:
    sys.stderr.write("[dampack_launcher] %s\n" % reason)
    sys.stderr.flush()
    raise SystemExit(exit_code)


def assemble(pack_dir: str) -> list:
    """装配 sys.path 并返回实际生效的前缀顺序（诊断/单测用）。

    顺序即优先级：包根（`import plugin`）→ `plugin/`（平铺模块）→ `vendor/`
    → `resources/` → SDK 自身 → 宿主 `DAMPACK_SHARED_SITE_DIR`（仅开发/兼容档）。
    venv 的 site-packages 由解释器自带，**不**往这里塞宿主环境路径。
    """
    pack_dir = os.path.abspath(pack_dir)
    plugin_dir = os.path.join(pack_dir, "plugin")
    vendor_dir = os.path.join(pack_dir, "vendor")
    resources_dir = os.path.join(pack_dir, "resources")

    if not os.path.isdir(pack_dir):
        _fail(EXIT_USAGE, "包目录不存在：%s" % pack_dir)
    if not os.path.isfile(os.path.join(pack_dir, "manifest.json")):
        _fail(EXIT_USAGE, "缺 manifest.json：%s" % pack_dir)
    if not os.path.isfile(os.path.join(plugin_dir, "__init__.py")):
        _fail(EXIT_LOAD, "插件入口缺失：%s/__init__.py（包必须带 plugin/__init__.py）"
              % plugin_dir)

    extras = [pack_dir] + [p for p in (plugin_dir, vendor_dir, resources_dir)
                           if os.path.isdir(p)]
    if SDK_ROOT not in extras and SDK_ROOT not in sys.path:
        extras.append(SDK_ROOT)
    sys.path[0:0] = extras

    shared = os.environ.get("DAMPACK_SHARED_SITE_DIR")
    if shared and os.path.isdir(shared):
        sys.path.append(shared)

    add_dll_directory = getattr(os, "add_dll_directory", None)
    if callable(add_dll_directory) and os.path.isdir(vendor_dir):
        try:
            add_dll_directory(vendor_dir)
        except OSError:
            pass
    return list(extras)


def entry_module(pack_dir: str) -> str:
    """由 manifest.entrypoint.args 推导要 import 的模块名（`plugin/__init__.py` → `plugin`）。"""
    try:
        with open(os.path.join(pack_dir, "manifest.json"), encoding="utf-8") as fh:
            manifest = json.load(fh)
        args = ((manifest.get("entrypoint") or {}).get("args") or [])
    except (OSError, ValueError):
        args = []
    for arg in args:
        rel = str(arg).replace("\\", "/").lstrip("./")
        if rel.endswith(".py"):
            rel = rel[:-3]
        if rel.endswith("/__init__"):
            rel = rel[: -len("/__init__")]
        if rel:
            return rel.replace("/", ".")
    return "plugin"


def probe(pack_dir: str, module: str) -> dict:
    """诊断：import 目标模块并回一行 JSON（stdout 唯一允许的非协议行）。"""
    out = _ensure_guard()
    assemble(pack_dir)
    try:
        mod = importlib.import_module(module)
    except Exception:  # noqa: BLE001
        _fail(EXIT_LOAD, "import 失败（%s）：%s"
              % (module, traceback.format_exc().strip().splitlines()[-1]))
    info = getattr(mod, "PACK_INFO", None)
    payload = {
        "ok": True,
        "module": module,
        "file": getattr(mod, "__file__", ""),
        "pack_id": (info or {}).get("id") if isinstance(info, dict) else None,
        "value": getattr(mod, "VALUE", None),
        "sys_path_head": sys.path[:4],
    }
    out.write(json.dumps(payload, ensure_ascii=False) + "\n")
    out.flush()
    return payload


def main(argv=None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv:
        _fail(EXIT_USAGE, "用法：dampack_launcher <pack_dir> [--probe MODULE]")
    pack_dir = os.path.abspath(argv[0])
    rest = argv[1:]

    if rest and rest[0] in ("-h", "--help"):
        out = _ensure_guard()
        out.write((__doc__ or "") + "\n")
        out.flush()
        return EXIT_OK

    if rest and rest[0] == "--probe":
        module = rest[1] if len(rest) > 1 else entry_module(pack_dir)
        probe(pack_dir, module)
        return EXIT_OK

    assemble(pack_dir)
    module = entry_module(pack_dir)
    try:
        mod = importlib.import_module(module)
    except Exception:  # noqa: BLE001
        _fail(EXIT_LOAD, "import 失败（%s）：%s"
              % (module, traceback.format_exc().strip().splitlines()[-1]))

    serve = getattr(mod, "serve", None)
    if callable(serve):
        hand_over_stdout()
        serve()
        return EXIT_OK
    entry_main = getattr(mod, "main", None)
    if callable(entry_main):
        hand_over_stdout()
        result = entry_main()
        return EXIT_OK if result is None else int(result)
    _fail(EXIT_LOAD, "插件模块 %s 缺 serve()/main()（协议要求导出 serve）" % module)
    return EXIT_LOAD


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

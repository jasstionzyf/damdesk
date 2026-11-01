#!/usr/bin/env python3
"""DAMDesk 行业包启动器（包与解释器之间的唯一胶水）。

用法：
    <解释器> -m dampack_launcher <pack_dir>              # 装配 sys.path 后跑插件 serve()
    <解释器> -m dampack_launcher <pack_dir> --probe MOD   # 诊断：装配后 import MOD 并回一行 JSON

解释器由宿主解析（协议 v1.0 + 2026-10-20 运行时裁决）：
    1) `<pack>/venv/bin/python3`（Windows `venv/Scripts/python.exe`）—— 每包独立 venv，装时创建；
    2) 内置 runtime 的 python（宿主传入）；
    3) `python3`（开发机兜底）。
**不做 --system-site-packages**：venv 就是隔离边界，跨包污染不可接受。

`sys.path` 装配顺序（前面的赢）：
    1) `<pack>`            —— 包根：`import plugin` 解析成包（`plugin/__init__.py`）
    2) `<pack>/plugin`     —— 脚本式包的平铺模块（`import utils` 这类，兼容 `python plugin/__init__.py`）
    3) `<pack>/vendor`     —— 包内私有依赖（可有可无）
    4) `<pack>/resources`  —— 只读资源（词表/模板），也放进来便于 import
    5) 宿主 `DAMPACK_SHARED_SITE_DIR`（**仅开发/兼容**，venv 形态下不设）
venv 的 site-packages 由解释器自己带，无需也不允许在此追加宿主环境路径。

退出码（宿主据此决定 `plugin_load_failed(<原因>)` 的用户可见文案）：
    0  正常
    2  用法/参数错误（缺 pack_dir、manifest 不存在）
    3  加载失败（插件入口缺失、import 抛异常、解释器不兼容）

铁律：stdout 只允许协议帧 / probe 的 JSON 行。启动器先把 fd1 存下来再把 fd1 接到
stderr，任何库里的 `print` 都落 stderr，不会污染协议流（协议 §7 铁律 4）。
"""
from __future__ import annotations

import importlib
import importlib.util
import json
import os
import sys
import traceback
from typing import NoReturn

EXIT_OK = 0
EXIT_USAGE = 2
EXIT_LOAD = 3

# 启动器先保护 stdout：保存真 fd1，再把 fd1 指向 stderr —— 装配期/import 期任何
# `print`（含库里的）都落 stderr。**交给插件运行前会把真 stdout 还回去**，因为插件
# 自己也要按协议 §9 做同样的 fd 保护，启动器不能提前把它吃掉。
_real_stdout_fd = os.dup(1)
os.dup2(2, 1)
# Windows 铁律：换下来的 stdout 对象必须留引用，不许被 GC —— 它析构时 `close(1)`，而
# CRT 里 1 号与 2 号 fd 共用同一个 OS 句柄，一关就把 stderr 一起打断（包内日志从此静默）。
_ORIG_STDOUT = sys.stdout
sys.stdout = sys.stderr
_raw_out = os.fdopen(os.dup(_real_stdout_fd), "w", encoding="utf-8", newline="\n")


def hand_over_stdout() -> None:
    """把真 stdout 还给插件进程（plugin 自己再 dup 一次做协议保护）。"""
    os.dup2(_real_stdout_fd, 1)
    sys.stdout = os.fdopen(os.dup(1), "w", encoding="utf-8", newline="\n")


def _fail(exit_code: int, reason: str) -> NoReturn:
    sys.stderr.write("[dampack_launcher] %s\n" % reason)
    sys.stderr.flush()
    raise SystemExit(exit_code)


# 诊断环境变量：`DAMPACK_THREAD_DUMP=<文件路径>` 时，启动器每 `DAMPACK_THREAD_DUMP_SECS`
# （默认 15）秒把**所有线程**的栈追加到该文件，文件里带 pid/时刻/线程名。
#
# 为什么放在启动器里（而不是各包自己加）：插件挂起时宿主看到的只有「请求写出后无应答」，
# 包内日志一条不增（卡 t_55ce222d 的真机现象）。faulthandler 的转储是 **C 层定时器**，
# 插件线程被 Python 层锁或阻塞 IO 卡住时它仍照常打印 —— 这是唯一不依赖宿主、不依赖被测
# 线程自己配合的证据来源。诊断设施本身绝不许影响插件：任何异常都静默吞掉。
def install_thread_dump() -> None:
    path = os.environ.get("DAMPACK_THREAD_DUMP")
    if not path:
        return
    try:
        import faulthandler
        import threading

        secs = float(os.environ.get("DAMPACK_THREAD_DUMP_SECS") or 15)
    except Exception:  # noqa: BLE001
        return
    try:
        fh = open(path, "a", encoding="utf-8", buffering=1)
        fh.write(
            "\n=== dampack thread dump watchdog ===\npid=%d argv=%r python=%s interval=%ss\n"
            % (os.getpid(), sys.argv, sys.version.split()[0], secs)
        )
        fh.flush()
        import faulthandler as _fth

        _fth.dump_traceback_later(secs, repeat=True, file=fh)
    except Exception:  # noqa: BLE001
        return
    # 让转储标注「这是哪一个线程的名字」—— faulthandler 打的是 id，主线程读的是 stdin。
    try:
        threading.current_thread().name = "dampack-main(stdin)"
    except Exception:  # noqa: BLE001
        pass


def assemble(pack_dir: str) -> list:
    """装配 sys.path 并返回实际生效的前缀顺序（诊断用）。"""
    plugin_dir = os.path.join(pack_dir, "plugin")
    vendor_dir = os.path.join(pack_dir, "vendor")
    resources_dir = os.path.join(pack_dir, "resources")

    if not os.path.isdir(pack_dir):
        _fail(EXIT_USAGE, "包目录不存在：%s" % pack_dir)
    if not os.path.isfile(os.path.join(pack_dir, "manifest.json")):
        _fail(EXIT_USAGE, "缺 manifest.json：%s" % pack_dir)
    if not os.path.isfile(os.path.join(plugin_dir, "__init__.py")):
        _fail(EXIT_LOAD, "插件入口缺失：%s/__init__.py（包必须带 plugin/__init__.py）" % plugin_dir)

    extras = [pack_dir] + [
        p for p in (plugin_dir, vendor_dir, resources_dir) if os.path.isdir(p)
    ]
    # 顺序即优先级：包根（import plugin）→ plugin/（平铺模块）→ vendor/ → resources/
    sys.path[0:0] = extras

    shared = os.environ.get("DAMPACK_SHARED_SITE_DIR")
    if shared and os.path.isdir(shared):
        # 仅开发/兼容档：排在最后，包内 vendor 永远优先
        sys.path.append(shared)

    # Windows：vendor 里的原生扩展需要 DLL 搜索路径
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
    """诊断用：import 目标模块并回一行 JSON（stdout 唯一允许的非协议行）。"""
    assemble(pack_dir)
    try:
        mod = importlib.import_module(module)
    except Exception:  # noqa: BLE001 —— 任何 import 失败都必须变成可读原因
        _fail(
            EXIT_LOAD,
            "import 失败（%s）：%s" % (module, traceback.format_exc().strip().splitlines()[-1]),
        )
    info = getattr(mod, "PACK_INFO", None)
    return {
        "ok": True,
        "module": module,
        "file": getattr(mod, "__file__", ""),
        "pack_id": (info or {}).get("id") if isinstance(info, dict) else None,
        "value": getattr(mod, "VALUE", None),
        "sys_path_head": sys.path[:4],
    }


def main(argv: list) -> int:
    if not argv:
        _fail(EXIT_USAGE, "用法：dampack_launcher <pack_dir> [--probe MODULE]")
    pack_dir = os.path.abspath(argv[0])
    rest = argv[1:]

    if rest and rest[0] in ("-h", "--help"):
        _raw_out.write(__doc__ or "")
        _raw_out.flush()
        return EXIT_OK

    if rest and rest[0] == "--probe":
        module = rest[1] if len(rest) > 1 else entry_module(pack_dir)
        payload = probe(pack_dir, module)
        _raw_out.write(json.dumps(payload, ensure_ascii=False) + "\n")
        _raw_out.flush()
        return EXIT_OK

    assemble(pack_dir)
    module = entry_module(pack_dir)
    try:
        mod = importlib.import_module(module)
    except Exception:  # noqa: BLE001
        _fail(
            EXIT_LOAD,
            "import 失败（%s）：%s" % (module, traceback.format_exc().strip().splitlines()[-1]),
        )

    serve = getattr(mod, "serve", None)
    install_thread_dump()
    if callable(serve):
        hand_over_stdout()
        serve()
        return EXIT_OK
    entry_main = getattr(mod, "main", None)
    if callable(entry_main):
        hand_over_stdout()
        result = entry_main()
        if result is None:
            return EXIT_OK
        return int(result)  # type: ignore[arg-type]
    _fail(EXIT_LOAD, "插件模块 %s 缺 serve()/main()（协议要求导出 serve）" % module)
    return EXIT_LOAD


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

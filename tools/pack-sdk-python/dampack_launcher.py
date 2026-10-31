"""`python -m dampack_launcher <pack_dir>` —— SDK 版启动器入口（与宿主 K02 启动器同契约）。

宿主 `pack-host` 用 `-m dampack_launcher <pack_dir>` 拉起插件，并把「含 dampack_launcher.py
的目录」塞进 PYTHONPATH。第三方包把 SDK 装进自己的 venv 后，site-packages 里就有本文件，
于是**宿主不需要随身带启动器**也能跑第三方包（也便于 `python -X importtime -m dampack_launcher`
这类离线排障）。

实现全部在 `dampack.launcher`，这里只做模块名兼容。
"""
from __future__ import annotations

import sys

from dampack.launcher import main

if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

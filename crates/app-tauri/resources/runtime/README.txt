内置 python 运行时（python-build-standalone win-x64）的落位点。

本目录内容**不入库**（约 72MB 未压缩）：构建前跑
  python3 scripts/stage_pack_runtime.py
把解释器解到 runtime/python/，并把 tools/dampack_launcher.py 拷到 ../pack-launcher/。

出包时 scripts/pack_runtime_gate.sh 会硬门禁：缺 runtime/python/python.exe
或缺随包启动器就不许出包（KI-28：纯净 Windows 没有 python3，缺了行业包永远起不来）。

⚠ 打包映射必须是**目录**（tauri.conf 里 `"resources/runtime": "runtime"`），不能写成 glob
（`resources/runtime/**/*`）：tauri 对 glob 条目只保留文件名、**拍平目录结构**，
实测会打成 `runtime/python.exe` 而不是 `runtime/python/python.exe` → 宿主解析不到解释器。
卡 t_5c9accc5 的第一次打包就踩了这个（见 reports/t_5c9accc5-KI28-RUNTIME.md §5）。

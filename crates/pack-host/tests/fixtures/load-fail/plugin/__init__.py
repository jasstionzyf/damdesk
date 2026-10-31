"""夹具：import 期就抛 ImportError（模拟 venv 缺依赖 / pip install 没跑成功）。

宿主应把启动器退出码 3 映射为 `plugin_load_failed(<原因>)`，且原因里带可读异常行。
"""
raise ImportError("No module named 'onnxruntime'（venv 未安装 requirements.txt）")

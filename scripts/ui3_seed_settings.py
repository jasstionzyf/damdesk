#!/usr/bin/env python3
"""COMPLIANCE-UI3 验收：把推理设置（VL 端点）写进真库的 settings 表。

为什么单独成文件（而不是脚本里的 heredoc）：heredoc 嵌在 `docker exec bash -lc "..."` 里，
引号/变量会被外壳先展开一遍，行为不可预测（第一版就因此静默没写进去，判定 0 行）。

用法：
  UI3_DATA=/tmp/ui3-webdata UI3_VL_BASE_URL=http://172.17.0.1:8091/v1 \
  UI3_VL_MODEL=qwen3.5-4b UI3_VL_API_KEY=... python3 scripts/ui3_seed_settings.py

口径：设置表由 app 自己的 migration 建（0001），所以本脚本**必须**在 app 首次启动之后运行；
只做 insert or replace，不建表（提前建表会和迁移打架）。
"""
import json
import os
import sqlite3
import sys

data = os.environ.get("UI3_DATA", "/tmp/ui3-webdata")
db = os.path.join(data, "dam.db")
if not os.path.exists(db):
    sys.exit(f"dam.db 不存在：{db}（先启动一次 app 让迁移建表）")

base_url = os.environ.get("UI3_VL_BASE_URL", "http://172.17.0.1:8091/v1")
model = os.environ.get("UI3_VL_MODEL", "qwen3.5-4b")
api_key = os.environ.get("UI3_VL_API_KEY", "")

settings = {
    "active_embedder": "openai-compat-embedding",
    "model_base_url": "https://hf-mirror.com/onnx-community/siglip2-base-patch16-224-ONNX/resolve/main",
    "auto_download": False,
    "embedding": {
        "base_url": base_url,
        "api_key": api_key,
        "model": "qwen3-vl-embedding-2b",
        "dims": 0,
        "timeout_secs": 120,
        "proxy": "",
    },
    "tag": {
        "enabled": True,
        "provider": "custom",
        "base_url": base_url,
        "api_key": api_key,
        "model": model,
        "prompt_template": "",
        "max_tokens": 1024,
        "timeout_ms": 120000,
        # 容器内直连宿主 vLLM：None = 走 env 代理（会炸），显式空串 = 直连
        "proxy": "",
    },
}

conn = sqlite3.connect(db, timeout=30)
conn.execute(
    "insert or replace into settings(key, value, updatedAt) values(?,?,cast(strftime('%s','now') as integer))",
    ("inference", json.dumps(settings)),
)
conn.commit()
row = conn.execute("select value from settings where key='inference'").fetchone()
conn.close()
got = json.loads(row[0])
print("seed ok: tag=%s/%s base_url=%s enabled=%s" % (got["tag"]["provider"], got["tag"]["model"], got["tag"]["base_url"], got["tag"]["enabled"]))

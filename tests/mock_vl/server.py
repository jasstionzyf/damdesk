#!/usr/bin/env python3
"""最小 OpenAI 兼容 mock 服务（D3 验收用：云 VL 打标 + 远程 embedding）。

用途：在没有 vLLM / 没有 GPU / 没有外网的环境里，端到端验证 desktop 的
「OpenAI 兼容客户端」链路——请求体形态、鉴权头、响应解析、错误路径。

启动：
    python3 tests/mock_vl/server.py --port 18080
然后：
    curl -s localhost:18080/v1/chat/completions -H 'content-type: application/json' \
      -d '{"model":"mock-vl","messages":[{"role":"user","content":"hi"}]}' | head -c 400

路由：
  GET  /v1/models              → 模型列表
  POST /v1/chat/completions    → 固定 JSON 三合一打标回复（默认带 ```json 围栏）
  POST /v1/embeddings          → 固定维度向量（默认 8 维，--dims 可调；确定性伪随机）
  POST /v1/chat/echo           → 回显请求（调试请求体形态）
均可通过 --dims / --fail 控制行为，用于验收「错误路径」。
"""

import argparse
import hashlib
import json
import random
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TITLE_CH = "草地上的金毛犬"
TITLE_EN = "Golden retriever on grass"
DESC = "一只金毛寻回犬趴在草地上，背景是绿树"
TAGS = [("狗", 0.98), ("草地", 0.80), ("宠物", 0.75), ("户外", 0.62), ("金毛寻回犬", 0.95)]


def tagging_reply(fenced: bool) -> str:
    payload = {
        "title_ch": TITLE_CH,
        "title_en": TITLE_EN,
        "description": DESC,
        "tags": [{"name": n, "score": s} for n, s in TAGS],
    }
    body = json.dumps(payload, ensure_ascii=False)
    return f"```json\n{body}\n```" if fenced else body


def deterministic_vector(seed_text: str, dims: int):
    """确定性伪随机向量（同输入同输出），单位长度。"""
    seed = int(hashlib.sha256(seed_text.encode("utf-8")).hexdigest()[:8], 16)
    rnd = random.Random(seed)
    v = [rnd.random() - 0.5 for _ in range(dims)]
    norm = sum(x * x for x in v) ** 0.5 or 1.0
    return [x / norm for x in v]


class Handler(BaseHTTPRequestHandler):
    dims = 8
    fail = False
    fenced = True
    requests_seen = 0

    def log_message(self, format, *args):  # 静音默认访问日志
        print("[mock_vl] " + format % args, flush=True)

    def _json(self, code, obj):
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            return json.loads(raw.decode("utf-8")) if raw else {}
        except Exception:
            return {"_raw": raw.decode("utf-8", "replace")}

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self._json(200, {"object": "list", "data": [{"id": "mock-vl", "object": "model"}]})
        else:
            self._json(404, {"error": {"message": f"no route {self.path}"}})

    def do_POST(self):
        Handler.requests_seen += 1
        body = self._read_body()
        if self.fail:
            self._json(500, {"error": {"message": "mock failure requested via --fail"}})
            return
        if self.path.startswith("/v1/chat/completions"):
            self._json(
                200,
                {
                    "id": "chatcmpl-mock",
                    "object": "chat.completion",
                    "model": body.get("model", "mock-vl"),
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": tagging_reply(self.fenced)},
                            "finish_reason": "stop",
                        }
                    ],
                },
            )
        elif self.path.startswith("/v1/chat/echo"):
            self._json(200, {"echo": body})
        elif self.path.startswith("/v1/embeddings"):
            inp = body.get("input")
            items = inp if isinstance(inp, list) else [inp]
            data = []
            for i, item in enumerate(items):
                # 图片走 image_url content part；文本是裸字符串
                if isinstance(item, dict):
                    key = json.dumps(item.get("image_url", {}), ensure_ascii=False)
                else:
                    key = str(item)
                data.append({"object": "embedding", "index": i, "embedding": deterministic_vector(key, self.dims)})
            self._json(200, {"object": "list", "model": body.get("model", "mock-embed"), "data": data})
        else:
            self._json(404, {"error": {"message": f"no route {self.path}"}})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18080)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--dims", type=int, default=8, help="embeddings 返回维度")
    ap.add_argument("--fail", action="store_true", help="所有 POST 返回 500（错误路径验收）")
    ap.add_argument("--no-fence", action="store_true", help="打标回复不加 ```json 围栏")
    args = ap.parse_args()
    Handler.dims = args.dims
    Handler.fail = args.fail
    Handler.fenced = not args.no_fence
    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"[mock_vl] listening on http://{args.host}:{args.port} dims={args.dims} fail={args.fail}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()

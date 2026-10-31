# D3 mock 服务（无外网 / 无 GPU 也能验收 OpenAI 兼容链路）

`server.py` 是一个最小 OpenAI 兼容服务，用于在没有 vLLM、没有 GPU、没有外网的环境里
端到端验证 desktop 的 **云 VL 打标** 与 **远程 embedding** 两条客户端链路：

| 路由 | 行为 |
|------|------|
| `GET /v1/models` | 模型列表 |
| `POST /v1/chat/completions` | 固定三合一打标回复（`title_ch/title_en/description/tags`），默认带 ` ```json ` 围栏 |
| `POST /v1/embeddings` | 确定性伪随机单位向量（默认 8 维，`--dims` 可调） |
| `POST /v1/chat/echo` | 回显请求体（调试 content parts / 图片 data URL 形态） |

## 启动

```bash
python3 tests/mock_vl/server.py --port 18080            # 正常
python3 tests/mock_vl/server.py --port 18080 --no-fence # 回复不带 ```json 围栏（解析健壮性）
python3 tests/mock_vl/server.py --port 18080 --fail     # 所有 POST 返回 500（错误路径）
```

## 手工验收（curl）

```bash
# 打标回复形态
curl -s localhost:18080/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock-vl","messages":[{"role":"user","content":"打标这张图"}]}' | head -c 300

# embedding 维度（--dims 8 → 8 个浮点）
curl -s localhost:18080/v1/embeddings -H 'content-type: application/json' \
  -d '{"model":"mock-embed","input":["一只狗"],"encoding_format":"float"}' | head -c 300

# 请求体形态（客户端把图片塞进 image_url content part 时）
curl -s localhost:18080/v1/chat/echo -H 'content-type: application/json' \
  -d '{"messages":[{"role":"user","content":[{"type":"text","text":"x"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}]}'
```

## 与 Rust 测试的关系

Rust 集成测试（`crates/engine-tag/tests/openai_compat_mock.rs`、
`crates/engine-embed/tests/openai_compat_mock.rs`）自带一个**进程内**极小 HTTP mock
（`tests/common/mod.rs`，仅 std），保证 `cargo test` 在完全离线时也能验证请求/响应契约。

本 python 服务是**跨语言/跨进程**的那一份：用于手工 curl 验收、前端联调、
以及需要长时间挂在某个端口给 app 手测（设置页填 `http://127.0.0.1:18080/v1`，
模型名 `mock-vl`，然后在图库点「AI 打标」）的场景。两者断言的是同一份契约。

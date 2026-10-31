# DAMDesk

**A local-first desktop DAM where industry processing flows are installable plugins.**

DAMDesk keeps your library on your machine — import, tag, deduplicate, search — and treats
*industry-specific* processing (e-commerce image compliance, real-estate listing checks, imaging
ops, segmentation, upscaling) as first-class **plugins** you install, switch, and roll back like
packages, not features someone has to merge into a monolith.

- **Local-first** — your assets stay on your disk; everything runs in one desktop process.
- **Plugin isolation** — every plugin is a separate OS process; a crash, a hang, or a stray
  `print()` in a plugin cannot touch the host or your library.
- **Capability gateway** — plugins have no direct network/filesystem access; everything goes
  through a whitelisted gateway.
- **Attributable results** — verdicts are three-state (`pass` / `fail` / `undetermined`) and every
  row carries the identity of the exact plugin build that produced it.

## Quick start (60 seconds)

```bash
pnpm -C web install
pnpm -C web dev            # frontend, Vite :5180, proxies /api to :8787
cargo run -p app-web       # backend, listens on 127.0.0.1:8787 (DAM_WEB_PORT), data in ./data
curl 127.0.0.1:8787/healthz
```

Or containerized: `docker compose -f deploy/docker-compose.yml up -d --build`.

## The plugin mechanism

This is the part DAMDesk exists for. Three moving pieces — **process model**, **capability
gateway**, **pack lifecycle** — plus an executable spec that keeps them honest.

### 1. Plugins are subprocesses speaking JSON-RPC over stdio

The host spawns each pack's entrypoint as a child process. The entire conversation is
line-delimited **JSON-RPC 2.0 on stdin/stdout**; stderr is free-form logs. Six methods make up the
whole contract:

| Method | Purpose |
|---|---|
| `pack.info` | static description: id, version, rules, capabilities |
| `health` | liveness probe used after spawn and before each job |
| `judge` | evaluate an asset against (a subset of) the pack's rules |
| `repair` | produce fixed variants for the rules a pack can repair |
| `report` | per-job accounting ledger emitted by the pack |
| `cancel` | cooperative cancellation; the host enforces a hard timeout (`cancel` → kill) |

A plugin in **any language** works — the transport is a pipe, not an ABI (official Python SDK today, anything else tomorrow).

### 2. The capability gateway: 15 endpoints, whitelisted per pack

Plugins cannot read files, call the network, or load models directly. The host exposes a
localhost HTTP gateway on a **random port with a one-time token** valid for that launch only, and
serves exactly 15 endpoints, each gated by a capability declared in `manifest.json`:

```
io/read                                → io.read
detect/u2net · detect/bbox             → detect.u2net
detect/ocr                             → detect.ocr
detect/white_stats                     → detect.white
op/crop · resize · letterbox · upscale · encode → op.geometric
op/watermark                           → op.overlay
op/replace_bg · op/inpaint             → op.generative
vl/judge                               → vl.judge        report/emit → report.emit
```

No declaration, no access — the gateway answers `403 capability_missing`, and the SDK maps it to a
typed error the pack must surface, not swallow. Image bytes never leave the host except through
these endpoints; the VL judge itself is called *by the host on behalf of* the pack.

### 3. Packs are artifacts: install, switch, roll back

A pack ships as a `.dampack` archive (manifest + entrypoint + resources + `requirements.txt`).
Installation is boring on purpose:

- **Install** = unpack into a versioned directory under the registry.
- **Upgrade** = install the new version, flip the `current` pointer.
- **Rollback** = flip the `current` pointer back. No rebuild, no reinstall.
- **Uninstall** = delete the version directory; the ledger row stays for audit.
- Optional **ed25519 signatures** for private registries.

Python packs get an isolated **venv per pack** at install time, so one pack's `onnxruntime` pin
never fights another's. Because dependencies live *inside* the pack, plugins can run native
libraries (vips, ONNX runtimes, C extensions) and **your private models** — no sandbox in the way.

### 4. Results are attributable

Every verdict row records the result identity:

```
packId@version#hash12     e.g.  io.example.pack@1.0.0#9f2c1ab77e43
```

`hash12` is the first 12 hex chars of the pack content hash: re-runs are comparable, regressions are bisable, "which build produced this verdict" always has an answer.

### 5. Conformance is part of the protocol, not a wish

`tools/pack-conformance/` is the executable spec: fake gateway + **real subprocess + real stdio
pipes**, **14 assertions** (01–12 protocol surface, 13/14 i18n fields). Every pack in any language
must pass all of them: stdout purity, method completeness, verdict shape, `undetermined` handling,
whitelist enforcement, error-code discipline, cancel semantics, artifact paths, job accounting —
plus four violation fixtures (stdout pollution, missing method, error-code misuse, no health)
that the suite proves it can catch.

### A complete plugin in ~20 lines

Based on the official Python SDK (`tools/pack-sdk-python`, also works offline as a 23 KB wheel):

```python
# plugin/__init__.py — minimal industry pack
from dampack import PackServer, Gateway, verdict

pack = PackServer(
    pack_id="io.example.pack", name="Example pack", version="1.0.0",
    rules=[{"id": "X1"}], capabilities=["io.read"],
    deterministic=True, timeout_secs=60)

@pack.method("judge")
def judge(params):
    path = params["asset"]["path"]
    meta = Gateway.from_env().io_read(path)     # the ONLY way to touch the image
    passed = meta["width"] >= 1000
    return {"asset_id": params["asset"]["id"],
            "verdicts": [verdict("X1", "pass" if passed else "fail",
                                 score=float(meta["width"]), label="min_width")]}

def serve():
    return pack.serve()

if __name__ == "__main__":
    serve()
```

`PackServer` generates `pack.info` from the same metadata (no drift), answers `health`/`cancel`
for you, protects stdout from stray `print()`, and validates every `judge`/`repair` response
before it reaches the host. Full example: `tools/pack-sdk-python/examples/ecommerce_py`.

## How DAMDesk compares

The axis that matters is not "has plugins" but "can a third party ship *image-processing* code
that runs native libraries and private models, with sane install/rollback semantics".

| Project | Third-party code plugins | Runs native libs & private models | Install / switch / rollback + consistency |
|---|---|---|---|
| **DAMDesk** | Yes — subprocess + line-delimited JSON-RPC, manifest as the only config surface | **Yes** — per-pack venv, any native dependency, private models | **Yes** — `.dampack` install / activate / rollback via one pointer, optional signatures, enforced by a 14-assertion conformance suite |
| immich | Yes — WASM (Extism) plugins calling host functions | No — the WASM sandbox cannot load native libs or arbitrary models | Weak — install = env var + drop files + restart; plugin API read-only; no public SDK on npm; no public spec |
| PhotoPrism | No public plugin system (extension request open since 2019) | — | — |
| LibrePhotos | No public plugin spec | — | — |
| Photoview | No public plugin spec | — | — |
| Damselfly | No public plugin spec | — | — |

*(Snapshot of public docs and repos at time of writing; mechanism described, not community size.)*

## Build & test

```bash
cargo test -p pack-host                          # host-side plugin runtime
bash tools/pack-conformance/run_all.sh           # conformance self-check (good pack + 4 violation variants)
bash scripts/pack_lint.sh packs/plugins/ecommerce  # schema lint of any pack dir (dev container)
bash tools/pack-sdk-python/run_tests.sh          # SDK: unit tests + conformance + per-pack venv + 10-image consistency
```

Five official packs ship under `packs/plugins/`: `ecommerce` (product-image compliance),
`realestate` (listing-image compliance), `imaging` (general image ops), `segment` (background
removal), `upscale` (4× super-resolution).

## Models & licensing

Model weights have their own terms. **DINOv3** (dedup embedding tower) is **not bundled** — it is
gated by Meta and must be obtained by you. The other bundled models (segmentation, OCR,
super-resolution, fonts) are under permissive Apache/BSD/OFL licenses. Full attributions live in
[`NOTICE`](NOTICE).

## Documentation & community

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — crates, layers, data flow
- [`docs/industry-pack-protocol.md`](docs/industry-pack-protocol.md) — the plugin protocol, field by field (authoritative)
- [`CONTRIBUTING.md`](CONTRIBUTING.md) · [`SECURITY.md`](SECURITY.md) — contributing, security reports
- Plugin SDK: [`tools/pack-sdk-python`](tools/pack-sdk-python) · conformance: [`tools/pack-conformance`](tools/pack-conformance)

## 中文摘要

DAMDesk 是 **local-first 的桌面 DAM**：图库、打标、去重、搜索在本机完成；行业处理流程
（电商主图合规、房产房源图合规、图片处理、抠图、超分）以**可安装插件**形式交付。

插件机制三件事：

1. **子进程 + stdio JSON-RPC**——插件是独立进程，六种方法（`pack.info`/`health`/`judge`/
   `repair`/`report`/`cancel`）构成完整契约；任何语言都能写，宿主不因插件崩溃或卡死受影响。
2. **能力网关**——插件不能直接碰文件/网络/模型，只能走宿主开的本地网关（随机端口 + 一次性
   token），**15 个端点按 manifest 能力白名单放行**，越权一律 `403`。
3. **包生命周期**——`.dampack` 归档，装新版本即切 `current` 指针，**回滚 = 把指针切回去**；
   每包独立 venv，可跑原生库与私有模型；结果行带 `packId@version#hash12` 身份，可审计可回比；
   conformance 套件 14 条断言是任何语言插件的上车门槛。

竞品对比见上表：immich 的 WASM 插件跑不了原生库与私有模型、装机靠改环境变量重启；
PhotoPrism 的插件诉求七年来仍是 open issue；LibrePhotos / Photoview / Damselfly 无公开插件规范。

跑起来：`pnpm -C web install && pnpm -C web dev` + `cargo run -p app-web`（容器：`docker compose -f deploy/docker-compose.yml up -d --build`）。

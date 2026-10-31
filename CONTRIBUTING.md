# Contributing to DAMDesk

Thanks for your interest — issues and pull requests are welcome.

## Development environment

Pick either:

- **Local**: Rust stable + Node 20 + pnpm. Backend: `cargo run -p app-web`; frontend:
  `pnpm -C web install && pnpm -C web dev`.
- **Container**: `docker compose -f deploy/docker-compose.yml up -d --build`.

Model weights are not in the repo; `scripts/test_gate.sh` self-heals by symlinking
`crates/app-tauri/resources/models/` to a shared model directory when present.

## Gate: four checks before every PR

| Check | Command |
|---|---|
| Format | `cargo fmt --all -- --check` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Tests (L0) | `bash scripts/test_gate.sh` |
| Conformance | `python3 tools/pack-conformance/run.py --plugin <pack_dir>` |

L0 must be fully green before any integration/E2E work (project rule). For plugin changes,
run the conformance suite against your pack; `bash tools/pack-conformance/run_all.sh`
self-checks the suite itself.

## Commits & pull requests

- Commit style: `type: summary` (`feat:`, `fix:`, `docs:`, `test:`), imperative, one topic.
- **PR flow**: the public repository is a mirror — accepted PRs are **cherry-picked onto the
  upstream internal history** by the maintainers, then re-published. This means: keep PRs
  small and single-topic, do not rebase/force-push after review starts, and expect commits
  to appear upstream with new hashes.
- CI runs the four checks above; a PR that fails any of them will not be picked.

## Contributing a processing plugin

Plugins are the intended third-party extension point — you do not need to touch the Rust
core:

1. Write your plugin against the protocol (`docs/industry-pack-protocol.md`; frozen v1.1).
   Python SDK: `tools/pack-sdk-python` (`dampack.PackServer`); minimal example:
   `tools/pack-sdk-python/examples/ecommerce_py/`.
2. Pass conformance: `python3 tools/pack-conformance/run.py --plugin <your_pack_dir>` —
   this is the onboarding bar for any language.
3. Package as `.dampack` (`scripts/k15_build_packs.py --out <dir>`) and install via
   `POST /api/compliance/packs/install`.
4. To ship your plugin in-repo, open a PR adding it under `packs/plugins/<your-pack>/`
   with a `README.md` and tests. Protocol-level changes (fields, methods, error codes)
   require bumping `api_version` and a design discussion first.

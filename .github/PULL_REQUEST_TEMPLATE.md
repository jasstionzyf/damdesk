<!--
  Merge requirements (enforced by review, CI must be green):
  - CI passes on the PR head (fmt / clippy -D warnings / test / web / conformance / pack_lint).
  - Anything that changes the plugin protocol must also update the conformance suite
    and bump `api_version` — see docs/industry-pack-protocol.md.
-->

## What does this PR change?

<!-- One or two sentences: the problem and the approach. Link related issues. -->

## Area(s) touched

- [ ] Host crates (`crates/`)
- [ ] Web UI (`web/`)
- [ ] Built-in packs (`packs/`)
- [ ] Plugin protocol / SDK (`docs/industry-pack-protocol.md`, `tools/pack-sdk-python/`, `tools/pack-conformance/`)
- [ ] Build / packaging / CI
- [ ] Docs only

## CI result

<!-- Paste the CI run summary (link or the failing/passing job list). Local runs are fine while a PR is open. -->

```
<paste CI result or local gate output>
```

## Does this change the plugin protocol?

<!-- If YES: the conformance suite must be updated in the same PR and `api_version` bumped in
     docs/industry-pack-protocol.md and every affected manifest. Protocol changes also need
     explicit maintainer sign-off. -->

- [ ] No, protocol-facing behavior is unchanged (`api_version` stays `1.1`).
- [ ] Yes — conformance suite updated in this PR, `api_version` bumped, and the change is described below:

  <!-- what changed, why it cannot be expressed as a pack-side change -->

## Checklist

- [ ] `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test` / `pnpm -C web test` green for the areas I touched
- [ ] `bash tools/pack-conformance/run_all.sh` green (any pack / protocol / SDK change)
- [ ] No secrets, internal URLs, or credentials in the diff
- [ ] New model weights are **not** committed (optional-download or user-provided instead)
- [ ] Docs updated where behavior is user-visible

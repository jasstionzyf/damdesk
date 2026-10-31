# Security Policy

## Reporting a vulnerability

Email **security@damdesk.local** (placeholder — replace with the real address before
public release). Include reproduction steps and affected version. Please do not open a
public issue for an unpatched vulnerability.

We respond within **48 hours** with an assessment and remediation plan. Fixes are
disclosed after a patch ships.

## Plugin trust model

Processing plugins are third-party code that runs on your machine. The runtime constrains
them as follows:

- **Signing is optional** (manifest `signature`; `signatureStatus: "unsigned"` is accepted
  by default). Treat unsigned packs from untrusted sources like any untrusted download.
- **Capability manifest**: every gateway endpoint requires a capability declared in the
  pack manifest (`io.read`, `detect.ocr`, `op.geometric`, `op.generative`, `vl.judge`,
  `report.emit`, …). Anything else is `403 capability_missing` — plugins cannot reach
  endpoints they did not declare.
- **Workdir boundary**: plugins read/write only their declared resources and the per-job
  work directory; the host passes data, plugins never get free filesystem access.
- **No network by default**: plugins have no direct network access; all host capabilities
  (OCR, segmentation, VL) are loopback calls to the local gateway with a one-shot token.
- **Process isolation**: each pack runs as its own subprocess with its own venv; a crash
  or hang affects one plugin, not the app.

## Scope

Vulnerabilities in the gateway permission boundary, pack installation/activation, or the
desktop shell are highest priority. Also welcome: reports about the protocol itself
(`docs/industry-pack-protocol.md`).

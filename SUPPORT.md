# Support Policy

The 0.x line is pre-production and receives best-effort security fixes only.
Compatibility may break while the origin model, resource governor, viewer state
machine, and protocol contract are hardened.

[docs/guides/production-support.md](docs/guides/production-support.md) states
the supported boundary and the platform claims.

No telemetry is enabled by default. Verification is a green `make ci` /
`make ci-full` run and nothing else — no review period, no external sign-off,
and no independent party reviews it.

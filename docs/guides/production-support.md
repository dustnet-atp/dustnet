# Production Support Matrix

| Component | 0.2 status | Stable target |
|---|---|---|
| ATP/AML scanner, parser, URI and origin | Preview | Supported |
| Terminal client and compositor | Preview (macOS verified) | macOS supported |
| Interpreted draw-only WASM host | Preview | Supported within budgets |
| Static `dustnetd` server | Preview | Supported |
| Accounts, email, boards, chat and links | Example only | Unsupported |
| Dynamic/operator-installed plugins | Out of scope | Unsupported |

## File descriptor limits

The server's default ceiling is 2048 concurrent connections, and each connection
holds a socket. The process therefore needs `RLIMIT_NOFILE` comfortably above
`--max-connections`, plus headroom for the files it serves.

Dustnet does not raise the limit for itself. Doing so requires `libc`, and the
workspace forbids unsafe code; a server that silently ran into a limit it
claimed not to have would be worse than one that documents the requirement.

- macOS defaults to a soft limit of 256. Raise it in the launching shell with
  `ulimit -n 8192`.
- Under systemd, set `LimitNOFILE=8192` in the unit file.

Lower `--max-connections` instead if raising the descriptor limit is not
available. The effective ceilings are logged at startup alongside the listening
address.

## Platform verification

Two platforms carry a verified gate for 0.2. Each is claimed at the scope of
the gate that actually ran on it, not at the scope of the gate that exists.

| Triple | `make ci` | `make ci-full` | Claim |
|---|---|---|---|
| `aarch64-apple-darwin` | verified | verified | **Supported** |
| `x86_64-unknown-linux-gnu` | verified | not run | **Supported for `make ci` scope** |
| `aarch64-unknown-linux-gnu` | not run | not run | Best-effort, untested |
| `x86_64-apple-darwin` | not run | not run | Best-effort, untested |

The Linux run is a real one on Ubuntu 24.04 (`rustc 1.94.0`), not an
inference: 1139 tests passed, including
`linux_pty_driver_propagates_child_status`, which exercises the util-linux
`script -q -e -c` driver that no macOS run can execute. Clippy, formatting,
the allocation and enforcement gates, the locked release build, `cargo deny`,
`cargo vet` and the install file list all pass there.

What Linux does **not** carry: Miri, AddressSanitizer and the fuzz smoke run,
all of which need the pinned nightly that was not installed on that machine.
Claims resting on those cover macOS alone.

`aarch64-unknown-linux-gnu` is deliberately still untested. The available
Linux machine is x86_64, and sharing an operating system with a tested
platform is not evidence about a different architecture — the aarch64/x86_64
split is exactly where alignment, atomics and endianness assumptions surface.

`x86_64-apple-darwin` remains an intended target for a later release,
contingent on a verified gate.

Compromised hosts and malicious operator-installed extensions are outside the
threat model.

## Deferred, and what deferring it costs

Named so the release does not over-claim, and so each reads as a decision
rather than an oversight.

| Deferred | What shipping without it costs |
|---|---|
| Deployment | No supported way to run this publicly. Deployment belongs to whichever repository owns the content. |
| Spans, correlation ids, structured export | The connection lifecycle is logged and per-request detail sits at `debug`, but nothing correlates a request across statements or exports to a log pipeline. |
| Health and readiness endpoints | An orchestrator has nothing to probe but a TCP connect. |
| Metrics | The counters exist internally — subscription budget, connection occupancy, per-IP fan-out — and are simply not exported. |
| TLS reload | Load-once. A certificate renewal needs a full restart, and there is no zero-downtime handoff, so renewal breaks live connections. |
| mTLS, SNI, multiple certificates | One certificate per process, no client authentication. |
| Client on-disk state | Sessions, cache and history are process-lifetime only. Every restart logs you out of every site and re-downloads every effect. Pinned certificates are the one exception and do persist. |
| Windows | `validate_key_permissions` is a silent no-op off unix, the pinned-certificate store is neither permission-checked nor created owner-only, and the viewer has no `not(unix)` termination flag. Not a compile error — a silent posture downgrade. |
| Hosted CI | Every gate is run by hand. A contributor's change is unverified until a maintainer runs it. |
| External review | **All verification is our own.** No outside party has examined this. Any public security claim must say so. |
| Signing, notarization, SBOM | Downloaded binaries cannot be verified as ours. |
| Homebrew, nix, deb, cargo-dist | Install is `cargo install` or a build from source. |
| Miri coverage of the WASM host | **Permanent.** The host is an interpreter and Miri is an interpreter; the pairing is impractical, not merely slow. The compensating controls are ASan, the fuzz targets, and the interpreter's own fuel and memory budgets. |

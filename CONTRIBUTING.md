# Contributing

Security and correctness changes take priority over feature work during the
0.2 hardening cycle. Keep production crates free of unsafe Rust and include
tests for hostile inputs and lifecycle transitions.

All verification runs locally — there is no hosted CI. Before submitting a
change, run the full gate:

```console
make ci
```

That covers formatting, warning-denied Clippy, crate-boundary checks, the
workspace test suite, the excluded tool crates, doctests, a locked release
build, and the advisory/licence/supply-chain audits. Use `make test` for the
fast inner loop while working.

Before a release, or after touching the parser, protocol, or WASM host, also
run the slow gates:

```console
make ci-full
```

That adds Miri, AddressSanitizer, and a bounded fuzz smoke run across all
seven targets, and runs only on `aarch64-apple-darwin` — the pinned nightly is
not installed elsewhere. `make ci` is verified on `aarch64-apple-darwin` and
`x86_64-unknown-linux-gnu`; see `docs/guides/production-support.md` for what each
platform's gate covers.

Security-sensitive changes need threat-model notes, and require the full gate
to be re-run: a previous green run does not carry forward to changed code. By
contributing, you agree that your contribution is licensed under MIT.

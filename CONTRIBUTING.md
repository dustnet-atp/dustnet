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
build, and the advisory/licence/supply-chain audits. It takes minutes, and
nothing in it takes longer than a build.

Use `make test` for the fast inner loop while working. It runs the `quick`
nextest profile, which omits the three tests that assert a production deadline
by waiting on a real clock — around ten seconds instead of twenty-five.
`make ci` runs the full profile, so those three are still gated before every
commit.

Before a release, or after touching the parser, protocol, or WASM host, also
run the slow gates:

```console
make ci-full
```

That adds Miri, AddressSanitizer, a bounded fuzz smoke run across every fuzz
target, and the fuzz campaign itself. It runs only on `aarch64-apple-darwin` —
the pinned nightly is not installed elsewhere.

Budget about an hour for it. The campaign is most of that: eight targets at
`FUZZ_SECONDS` each, forty minutes at the default 300. It appends one row per
target to `verification/fuzz-campaign.tsv`, keyed on a fingerprint of every
source under `crates/` and `fuzz/fuzz_targets`, and `make fuzz-campaign-check`
then requires a row per target at the current fingerprint.

That fingerprint is why the campaign belongs to this tier and not to `make ci`:
**any** edit under `crates/` invalidates every row at once, so the requirement
cannot sit in front of a command you run before every commit. If you need the
rows refreshed without the rest of ci-full, `make fuzz-campaign` writes them and
`make fuzz-campaign-check` verifies them. Shorten a working run with
`make fuzz-campaign FUZZ_SECONDS=60`; a release should use the default.

`make ci` is verified on `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`;
see `docs/guides/production-support.md` for what each platform's gate covers.

Security-sensitive changes need threat-model notes, and require the full gate
to be re-run: a previous green run does not carry forward to changed code. By
contributing, you agree that your contribution is licensed under MIT.

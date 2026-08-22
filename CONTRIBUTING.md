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

That adds Miri, AddressSanitizer, and a bounded fuzz smoke run across every
fuzz target. It runs only on `aarch64-apple-darwin` — the pinned nightly is not
installed elsewhere. Budget about twenty minutes, most of it Miri.

## Fuzzing

The campaign is its own tier, and is **not** part of any release gate:

```console
make fuzz-periodic
```

Run it when the code a fuzz target actually exercises has changed — the parser,
scanner, protocol, URI or serializer. Eight targets at `FUZZ_SECONDS` each,
forty minutes at the default 300. It appends one row per target to
`verification/fuzz-campaign.tsv` and then verifies them; shorten a working run
with `make fuzz-campaign FUZZ_SECONDS=60`, and use the default for a run you
intend to record.

It sits outside `ci-full` because of how the rows are keyed: on a fingerprint
of **every** source under `crates/` and `fuzz/fuzz_targets`. Any edit anywhere
invalidates all eight targets at once, so a change to client-side session
storage — which no fuzz target can reach — demanded a fresh parser campaign.
A requirement paid constantly that tells you almost nothing is one that gets
worked around rather than read.

The honest cost of that: a release can go out whose parser has not been fuzzed
at its exact fingerprint. The `code` column in the campaign log says which
source each row covers, so what a given release is missing is answerable rather
than assumed. `make fuzz-campaign-check` still fails when a row is missing —
what changed is that no gate runs it for you.

`make ci` is verified on `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`;
see `docs/guides/production-support.md` for what each platform's gate covers.

Security-sensitive changes need threat-model notes, and require the full gate
to be re-run: a previous green run does not carry forward to changed code. By
contributing, you agree that your contribution is licensed under MIT.

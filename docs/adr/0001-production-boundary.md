# ADR 0001: Production Boundary and Staged Release

Status: accepted, staging amended

Dustnet will harden privately under 0.x, run a public pre-production RC for at
least 90 days, and freeze ATP/AML 1.0 only after the assurance gates pass.

**Amended.** The 90-day RC, the non-maintainer sign-offs and the written
maintainer assessment were subsequently removed: they were ceremony no gate
could check. Releasing is now gated on a green `make ci` / `make ci-full` run
at its stated scope and nothing else — see
[CONTRIBUTING.md](../../CONTRIBUTING.md). The production boundary below is
unaffected and stands as decided.

The production boundary contains the core protocol/parser/origin model, client
and WASM host, and static server. Social applications and dynamic plugins are
examples. This reduces the trusted server surface and prevents examples from
being mistaken for supported infrastructure.

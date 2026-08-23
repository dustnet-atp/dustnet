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

**Amended again**, 2026-08-21: `dustnet-server` now provides three optional
hooks — include resolution, form submission, session resolution — and a server
that installs them generates content and accepts writes.

**Amended a third time**, 2026-08-23: the `examples/unsupported-social`
prototype referred to below has been deleted. It was doing two jobs with
opposite lifecycles. As a worked example of a social application it was
superseded by a real one built on the hooks, in its own repository, the way this
ADR prescribes — and a superseded example is worse than none, because someone
may copy it. As an exhibit of the string-concatenation injection defect that
motivated `dustnet_core::serialize` it was still worth something, and that value
is what the deletion costs: the defect is now described in `serialize.rs` and
`docs/spec/07-security.md` rather than demonstrated. The boundary is unchanged;
what changed is that nothing in this repository sits outside it any more.

This is a narrowing of the original decision rather than a reversal of it, and
the distinction is what the amendment turns on:

- **What was rejected stays rejected.** No plugin registry, no
  operator-installed extensions, no dynamic code loading, no accounts or social
  application inside the boundary.
- **`dustnetd` did not change.** It installs no hooks, so it authenticates
  nobody, generates nothing, and still refuses `INPUT` with 405. That is held to
  tests rather than asserted: `without_a_handler_input_is_still_refused` and
  `without_a_resolver_includes_are_served_verbatim`.
- **The boundary moved by three traits, not by an application.** What is
  supported is what the hooks guarantee — escaped generation, bounded
  submissions, session tokens that never reach a handler. What a site builds on
  them is the site's, and lives in the site's repository.

Why amend rather than keep social applications wholly outside: the original
framing left a site with no supported way to serve dynamic content at all, so the
only worked example was a prototype whose approach to generating AML —
string concatenation with hand-remembered escaping — had a live injection defect
in it. Providing a narrow, tested surface is a smaller risk than leaving that as
the only example to copy.

`verification/threat-model.json` names the adversaries this newly faces, and
`docs/spec/07-security.md` separates what the library guarantees from what a site
must still do for itself.

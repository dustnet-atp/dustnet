# Known defects

Defects found and not yet fixed, each with what it costs to leave.

This is a register of things that are **broken**. What is merely **absent** is
the "Deferred, and what deferring it costs" table in
[`../docs/guides/production-support.md`](../docs/guides/production-support.md).

Nothing here is read by the build, and that is the weakness of the format: a
prose register is exactly the kind of document that rots, which is why the
release plan was retired and its claims attached to what enforces them. A
defect has less to attach to — you cannot gate on a bug's continued existence —
but the honest end state for each entry is a test that fails, or one that pins
the current bad bound so a fix has something to flip. Until an entry has that,
it is only a note.

A defect leaves this file when it is fixed, or when it is reclassified as
accepted with a written reason — "not got to it yet" is not a reason.

**The register is currently empty.** That is a statement about what has been
found, not about what is there.

## Blind spots this register has cost us

Kept because the defects left and the blind spots did not.

Both of the last two entries here were shapes no gate was looking for. Defect 1
— layout exponential in `[box]` nesting depth, `T(d) = 2·T(d-1)` from a `Fit`
box measuring its subtree and then laying it out again; fixed 2026-08-21 by
memoising the measure pass per node per layout pass — was **compute**. The one
before it — `trim_owned_string` panicking on whitespace-only element content,
now covered by `trim_owned_string_handles_whitespace_overlap` — was what remote
content may **index**: `value.drain(..start)` allocates nothing.

Every allocation gate asks what remote content may *allocate*. Neither of those
allocates: node count stays small throughout, nothing grows unboundedly, and the
resource governor bounds allocation rather than work. So no allocation-shaped
check could have seen either. The fuzzer found both.

The generalisation is the part worth keeping: **an allocation-shaped gate cannot
see a compute-shaped defect**, and `tools/allocation-audit` is allocation-shaped
by construction. Coverage for those has to be asserted directly.
`verification/threat-model.json` now cites
`layout_at_max_depth_completes_within_time_bound` alongside
`rejects_deep_nesting` for **Malicious AML → bounded resource use**: the two
prove different halves of it, that beyond `MAX_DEPTH` is refused, and that *at*
`MAX_DEPTH` — the case that is accepted, and that took twenty minutes — cost
stays bounded.

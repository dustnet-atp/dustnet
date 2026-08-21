# Known defects

Defects found and not yet fixed, each with what it costs to leave.

This is a register of things that are **broken**. What is merely **absent** is
the "Deferred, and what deferring it costs" table in
[`../docs/guides/production-support.md`](../docs/guides/production-support.md).

Nothing here is read by the build, and that is the weakness of the format: a
prose register is exactly the kind of document that rots, which is why the
release plan was retired and its claims attached to what enforces them. A
defect has less to attach to — you cannot gate on a bug's continued existence —
but the honest end state for each entry below is a test that fails, or one that
pins the current bad bound so a fix has something to flip. Until an entry has
that, it is only a note.

A defect leaves this file when it is fixed, or when it is reclassified as
accepted with a written reason — "not got to it yet" is not a reason.

---

## 1. Layout is exponential in `[box]` nesting depth

**Found** 2026-08-21, by a weighted fuzz campaign (`fuzz_pipeline`, corpus
input `93cd17c1c441389b8bb4a93242edf8cede0d6852`).
**Severity** low in effect, but it falsifies a threat-model claim — see
*Evidence gap* below, which is the part worth caring about.

### What happens

`layout_box_node` computes a `Dimension::Fit` box's height by calling
`measure_children_height_scene`, which performs a **full recursive layout of
the whole subtree** into a throwaway buffer, reads the height off it, discards
the work — and then lays those same children out again for real.

`crates/dustnet-client/src/compositor/layout/engine.rs`:

```rust
let content_height = if h == Dimension::Fit {
    measure_children_height_scene(inner_w, …, scene, &children, …)
} else { 0 };
```

Every `Fit` box therefore walks its subtree twice: **T(d) = 2·T(d−1) → O(2ᵈ)**.
`Fit` is the default — `data.height.unwrap_or(Dimension::Fit)` in
`layout/kinds/flow.rs` — so every `[box]` without an explicit `h=` is affected.

### Measured

Release build, one `[box border=rounded]` per level, `hello` at the centre.
Identical curve for well-formed and unclosed input, so this is the normal
layout path and not error recovery.

| Depth | Layout | Ratio | Same depth with explicit `h=` |
|---|---|---|---|
| 16 | 145 ms | 1.86 | 0.119 ms |
| 20 | 883 ms | — | 0.112 ms |
| 23 | 3.9 s | 1.80 | — |
| 26 | 26.1 s | 1.89 | — |
| 32 (`MAX_DEPTH`) | ~20 min (extrapolated at 1.89/level) | | 0.112 ms |

Node count grows linearly (10 → 34) across that range: the work is
re-measurement, not more content. Setting an explicit height removes the
measure pass and the cost with it — that is the confirmation of the cause.

### Reproducer

683 bytes, well-formed, accepted by `dustnet check` (`MAX_DEPTH` is 32, so 31
nested boxes is conforming):

```python
src = '[page mode=document]\n' + '[box border=rounded]\n'*31 + 'hi\n[/page]\n'
```

Do not run layout on it casually; it takes roughly twenty minutes. Depth 20
reproduces the shape in under a second.

### What it costs to leave

Little, in practice. It is client-side only: `dustnetd` serves static files and
never lays out AML, so no server can be hung by this. It needs the victim to
navigate to the attacker's page, and recovery is killing the process — no code
execution, no data loss, nothing persistent. It is a browser tab that hangs.

It costs real content nothing at all. Across all 70 `.aml` files in this
repository the maximum nesting depth is **5** (median 2), which lays out in
0.16 ms. The only deeper file is the conformance fixture built to exceed the
limit. Nothing legitimate is near the cliff.

It does cost fuzzing throughput: `fuzz_pipeline` runs at 43 executions/second
against `fuzz_uri`'s 144,721, because corpus inputs containing nested `Fit`
boxes cost hundreds of milliseconds each. The target most worth running deeply
is the one this makes slowest.

### Evidence gap — the part that matters

`THREAT_COVERAGE` in `tools/allocation-audit` maps **Malicious AML** →
**bounded resource use** → control *Resource Exhaustion* → evidence
`rejects_deep_nesting`.

That test builds a 35-deep document and asserts the parser emits `E009`. It
proves depth **greater than** `MAX_DEPTH` is rejected. It says nothing about
the cost of depth 32, which is *accepted* — and which is the case that takes
twenty minutes.

So the cited evidence does not cover the claim it is cited for. Until this is
fixed, the threat model asserts a bound that does not hold. That is worse than
the defect: a security claim whose evidence points somewhere else stops anyone
looking.

### Options, cheapest first

1. **Lower `MAX_DEPTH`.** It is arbitrary headroom over real content's depth 5.
   16 caps the worst case at 145 ms — a hang becomes jank — for one constant,
   one line of `docs/spec/05-conformance.md`, one row of
   `docs/spec/03-markup.md`, and a conformance fixture. Watch the headroom:
   component expansion adds depth, so `[def]`-heavy pages need checking before
   picking the number.
2. **Honest evidence.** A test asserting layout at `MAX_DEPTH` completes within
   a time bound, cited from `THREAT_COVERAGE` in place of
   `rejects_deep_nesting`. This is the check that would have caught it, and it
   is worth having whichever fix lands.
3. **Memoise the measure pass.** Cache measured height per `(node_id, width)`;
   collapses O(2ᵈ) to O(d) with no output change. The key is sound — a node has
   one parent, measure always uses a height-1 buffer, and `inner_w` matches
   `inner_w_actual` in the non-degenerate case — but that is four things that
   have to stay true. It also adds a collection under
   `crates/dustnet-client/src/compositor/layout/`, which is a
   `LOCAL_ALLOCATION_ROOT`, so it must reserve capacity or carry a written
   exemption.
4. **Remove the second traversal.** Lay children out once and take the height
   from that pass, deleting `measure_children_height_scene`. The right end
   state, and more available than it looks: `_inner_h` from
   `draw_border_with_joints` is already discarded, and the children's origin and
   width depend on `box_w` and the border inset, not on `box_h`. Two real
   risks: `Dimension::Fill` height resolves against `buf.height`, so laying
   children out before `buf.ensure_height(box_y + box_h)` changes what a
   `h=fill` descendant sees; and `draw_border_with_joints` returns `(x, y, 0,
   0)` for degenerate sizes, which is a genuine height-to-width dependence.
   Gate it on the parity harness (`compositor/parity_tests.rs`, golden ANSI plus
   golden cell grids) and add a fixture for `h=fill` inside `h=fit`, which the
   existing nine probably do not cover.

### Blind spot this sits in

The last remote-input defect here — `trim_owned_string` panicking on
whitespace-only element content, now covered by
`trim_owned_string_handles_whitespace_overlap` — survived because every
allocation gate asks what remote content may *allocate*, and none asked what it
may *index*: `value.drain(..start)` allocates nothing. This is a third
category, what it may **compute**. Node count stays small throughout, nothing
is allocated unboundedly, and the resource governor bounds allocation rather
than work, so no allocation-shaped check could have seen it. The fuzzer found
it, as it found the last one.

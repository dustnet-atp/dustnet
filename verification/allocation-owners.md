# Allocation-owner inventory

`verification/allocation-owners.tsv` is the machine-readable source of truth for the first
mechanically covered allocation authorities. Each row names a stable owner ID,
the exact Rust field that retains heap storage, its trust source and lifetime,
the enforced bound, governor category and admission boundary (when present),
the failure and release behavior, implementation status, and an accounting
test for every row classified as `governed`.

Run the gate with:

```sh
cargo run --manifest-path tools/allocation-audit/Cargo.toml -- --check
```

The checker lexes the named authority structs and registered wrapper tokens and
fails when a directly declared heap-owning field is missing from the inventory,
a row is duplicated or stale, a classification is malformed, or a governed row
names a missing test. It also registers heap-producing functions that do not
retain their result in a named field, pins the PLAN inventory counts, and checks
that every production transport send/receive path contains body-size, flag,
typed-body, capability, and state validation calls that are error-propagating
and ordered before dispatch. The enforced surface is:

- `SessionStore` and `SiteSessionStore`
- `RawFrame` and every heap-owning ATP message (`HelloMessage`,
  `WelcomeMessage`, `GetMessage`, `PageMessage`, `RedirectMessage`,
  `ErrorMessage`, `InputMessage`, `SubscribeMessage`, and `UpdateMessage`)
- `AtpClient`, `ResourceCache`, `ResourceCacheEntry`, `SharedResource`,
  `SharedResourceAllocation`, `ScopedResource`, `AtpConnection`,
  `ViewerModel`, `HistoryEntry`, `PendingHistoryActivation`, and
  `PendingHistoryCommit`
- `LoadedPage`, `TerminalRuntime`, `PreparedWasmArtifact`,
  `PreparedWasmBatch`, `RegionBuffer`, `RegionBufferEntry`, and `RegionBuffers`
- `Scene`, `RelayoutJournal`, `Invalidation`, `Compositor`, and
  `AnimationRuntime`
- `EventDispatcher`, `CommandLine`, and `ErrorLog`
- `AtpServerStream` and `StaticSubscription`
- registered transient producers for frame allocation/encoding, every ATP
  message serializer, PAGE body decoding, WASM dependency discovery, form
  serialization and synchronized terminal HUD
  frame staging

The status values are deliberately strict:

- `governed`: allocation is admitted and lifetime-accounted; a real test anchor
  is mandatory.
- `bounded`: an enforced ceiling exists, but no governor lease is required or
  claimed by the row.
- `local`: the owner is not remotely influenced.
- `gap`: admission, accounting, or recoverable allocator handling remains
  incomplete. Its admission column must start with `missing:`.

This inventory does not convert a `gap` into an exemption. The checker proves
spelling, coverage of the registered surface, status syntax, live governed test
names, PLAN count consistency, and current transport validation structure; it
does not infer that prose admission or release claims are semantically correct.
Independent tests and review remain required, as does owner-keyed
allocator-failpoint expansion.

## The struct registry is closed

The checker discovers every struct declared under `crates/*/src`, skipping
`#[cfg(test)]` items and files reached only through a
`#[cfg(test)] #[path = "..."] mod` declaration. Each discovered struct that owns
heap storage must be either present in `verification/allocation-owners.tsv` or listed in the
`EXEMPT` table in `tools/allocation-audit/src/main.rs` with a reason. The gate
fails closed on anything in neither, and also rejects an exemption or an
authority whose struct has disappeared or no longer owns heap storage.

No counts are repeated here. A prose copy of a machine-checked number drifts:
this table once said 229/164/107 while the checked marker said 231/165/108,
because only one of them was ever checked. Ask the tool instead — it is the
only thing that knows.

Run `cargo run --manifest-path tools/allocation-audit/Cargo.toml -- --report`
to list the unclassified set while working.

### What this does not cover

Closure applies to the **struct** surface. Two things remain outside it:

- **Function-local allocation.** A `Vec` built inside a function body and
  returned or consumed there is not a struct field, so discovery cannot see it.
  The backlog this generated is closed, and `repository_local_backlog_is_closed`
  in `tools/allocation-audit` fails if it reopens.
- **Enum payloads.** Heap storage carried in an enum variant is reached only
  when some struct field holds that enum. `NodeKind` is the significant case,
  and it is accounted through `Scene::retained_string_capacity`.

### On the exemptions

The exemptions are not waivers. Each names the row or mechanism that
accounts for the struct, and they fall into a few groups: AML AST payload
bounded under `transient.parser_result`; `NodeKind` payload summed by
`Scene::retained_string_capacity` into the `AstStrings` admission; nested slots
of a registered authority (`ActiveSubscription`, `PendingUpdate`); governed
layout transients that carry their own lease; bounded fallible writers; opaque
reducer-issued identities; and local, non-remote configuration.

Two of those groups carry most of the weight and should be re-verified if
either mechanism changes: that `transient.parser_result` genuinely bounds AST
construction, and that `Scene::retained_string_capacity` genuinely sums node
payload strings.

## Module isolation

The same tool checks `MODULE_ISOLATION`: rules of the form "nothing reachable
from this module may reach these modules", evaluated over the crate-internal
`use` graph rather than by searching source text.

Currently one rule: nothing under
`crates/dustnet-client/src/compositor/animate` — including everything it
imports, and everything those modules declare as submodules — may reach
`crate::client`, `crate::transport`, `crate::viewer` or `crate::session_store`.
That is the mechanical form of "animation construction has no network access
and cannot drive the reducer"; remote WASM reaches an animation adapter only as
reducer-prepared bytes.

Reaching a module requires importing it, so a rename cannot defeat the rule and
a mention in a comment cannot trip it. Violations report the import chain, for
example `crate::compositor::scene -> crate::transport::AtpConnection`. The
import reader handles flat, grouped and multi-line `use` forms, including
`use crate::{a::B, c::D};`.

See `verification/threat-model.json` for the model that motivates the accounting.

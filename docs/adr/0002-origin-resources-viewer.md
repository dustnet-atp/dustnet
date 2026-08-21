# ADR 0002: Origin Ownership, Resource Rejection, and Viewer Reduction

Status: accepted

Every connection-derived object is owned by a canonical `Origin`, including
its transport-security context. Navigation retires the old origin and its
sessions, subscriptions, queued updates, and connection state. Only a
same-origin transport reconnect may replay subscriptions.

`ViewerModel` is the sole authority that creates page generations, request
IDs, operation owners, live-region projection identities, and control tokens.
Their fields and constructors are crate-private; public reducer consumers get
read-only accessors. `ViewerModel` state is likewise externally read-only and
`reduce` is its mutation boundary. The raw ATP client module and transport
response types are internal implementation details rather than a second Rust
API surface.

Remote render allocations use fallible buffer construction. A scene may own at
most 1,048,576 cells in aggregate; crossing that boundary discards the hostile
scene and displays client-owned AML. Resource, update, history, and WASM limits
remain lower independent ceilings and are eviction-first where retention is
optional.

Viewer input is normalized as `ViewerEvent`. `ViewerModel::reduce` performs
deterministic state transitions and emits `ViewerEffect`; asynchronous network,
layout, WASM, and terminal work remains in the runner. Every asynchronous event
and effect carries one `OperationOwner` rather than parallel scope/request
fields. Layout and WASM work complete in two steps: the runner prepares an
owner-tagged result, and only a reducer-issued activation effect may install it.

`compositor/terminal.rs` is the sole production reducer dispatcher. The
transport, navigation, presentation, rendering, and lifecycle modules may
return events or execute effects, but an architecture test rejects direct
`.reduce(` calls and mutable `ViewerModel` parameters in those modules. Input
and focus projection additionally require a generation-scoped `ControlToken`,
so controls captured before navigation cannot mutate the replacement page.
Production ownership modules receive a sealed `ReducerPort`: it dereferences
only to an immutable model snapshot and offers mutation solely as event
dispatch. The boundary test also rejects the former mutable `LifecycleModel`
alias so type renaming cannot reopen reducer ownership.
Runtime completions are reduced before unrelated queued effects continue, so
dependent effects retain FIFO order (notably history release before bounded
error-page activation). Validated live-update payloads remain buffered by exact
owner until `ApplyUpdate`; stale or unsolicited payloads are discarded without
touching the active scene.

Presentation allocation failure is also an ordered reducer protocol. The
runtime first evicts one LRU resource and retries rendering, then evicts the
oldest non-current logical history entry and its matching presentation artifact
and retries again. It never evicts current history. Only after both tiers are
exhausted may reducer-issued effects retire page work and install the bounded,
independently governed client error page.

The runtime also executes the full navigation pipeline: `Fetch`/`Submit`
produce validated domain completions, `Parse` discovers exact-owner WASM
dependencies, `LoadWasm` buffers scoped resources, and `PrepareLayout` stages
the page. Cached history follows the same parse/dependency/layout stages.
Neither a transport completion nor a prepared page can mutate the active
projection without the reducer's matching activation effect.

Resize, timer advancement, deferred navigation, subscription retirement, and
scoped input/focus projection also execute through the same runtime boundary.
Scoped `PresentationAction` events cover authored page/focus/animation events,
delayed actions, focus and scroll movement, panel/details relayout, animation
paints and patches, local overlays, page-transition capture/install, and the
final render authorization. A stale page scope therefore cannot mutate the
active scene through an old presentation action.
Local pages use an ownerless timer effect because they have no remote origin;
remote timers retain an exact outstanding `OperationOwner` through activation.
The activated page retains its prepared WASM byte map, so authored panel or
details relayout can rebuild animation topology without transport access or a
second reducer ingress path. The animation runtime has no reducer capability;
it also has no client or network capability. Remote modules must arrive through
the reducer's `LoadWasm` effect, while explicitly local rendering may load from
its configured filesystem root. Architectural tests enforce both boundaries.

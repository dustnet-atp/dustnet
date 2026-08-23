# Compositor Architecture

*Architecture of the Dustnet reference client's display subsystem as implemented in
`crates/dustnet-client/src/compositor/`. This document describes how the reference client
realizes the rendering behavior specified in
[04-rendering.md](../spec/04-rendering.md). The spec is authoritative for
what a conforming client must produce on screen; this doc is
authoritative for how this codebase is structured internally.*

*The architecture described here is fully implemented. `Scene` is
authoritative for the node tree, buffer ownership, focus target,
hit-testing, event bindings, panel active state, and layout input; layout
is driven by `layout_scene(&scene, …)`, with no AST read after
`build_scene`. The composite side is load-bearing end to end — every cell
the user sees originates from a scene node the composite walk visits, with
no bypass paths.*

*Editing rule:*
- *`04-rendering.md` — edit when the rendering contract changes
  (authors or implementers need to know something new).*
- *This document — edit when the internal structure of
  `crates/dustnet-client/src/compositor/` changes.*

## Motivation

This structure replaced an earlier display subsystem that had the right
ideas without making them load-bearing. The gaps it was built to close,
recorded because they explain why the model looks as it does:

- **Placement is a side-effect of layout.** Layout functions return `()`
  and communicate through mutation of a flow cursor (`LayoutCtx.y`) and
  a shared `CellBuffer`. "Where did this element land" is reconstructed
  from cursor deltas, which works only when flow advancement is a faithful
  proxy for placement. Absolute positioning breaks the proxy.
- **The pre-scene compositor was optional.** The retired root-crate renderer
  defined a layer stack, but most code paths bypassed it — panel
  transitions extract sub-buffers and blit directly; animations write
  into a shared grid.
- **"Rectangle with an id" is reinvented three times.** `PanelRegion`,
  `AnimationRegion`, `LiveRegionInfo` are structurally identical — all
  four-tuples of `(x, y, w, h)` with an identifier — each re-declared
  separately because placement is not a first-class concept.
- **State changes trigger full-page re-layout.** Flipping one panel
  re-runs layout from the document root, requiring snapshots of the old
  buffer and old region metadata to drive transitions from what no longer
  exists.
- **The per-tick loop is the only unifier for time-based code.** Frame
  animations, tweens, text effects, panel transitions, page transitions,
  live-region polling, scroll smoothing, and sticky header compositing
  all live in one 2,108-line file because they share a tick, not because
  they share a concern.
- **Transitions cannot handle shape-mismatched states.** The per-cell
  blend `blend(old[i,j], new[i,j], t)` has no definition for cells that
  exist in only one of the two states.

These were not separate bugs — they were the same architectural gap
expressed in several places. A system that treats the display as a
*persistent scene composited from independent layers* has none of them.

The scanner, parser, ATP protocol, WASM sandbox semantics and AML surface
language were unchanged by the redesign: it was scoped to the client-side
display subsystem.

## One-sentence summary

The display subsystem is a **persistent scene graph of layered nodes**,
mutated by **patches**, rendered by a **composition pass over dirty
regions only**.

## Core Model

### Scene

A `Scene` is the persistent root of all display state. It is constructed
once from an AML `Document` and mutated in place thereafter. Navigating
to a different page replaces the scene wholesale; staying on a page
patches it.

```rust
pub struct Scene {
    root: NodeId,
    nodes: SlotMap<NodeId, Node>,
    invalid: NodeIdSet,      // nodes whose layout or content needs recomputing
    dirty: DirtyRegions,     // screen-space regions that need re-compositing
    animations: Vec<Animation>,
    focus: Option<NodeId>,
    scroll: ScrollState,
}
```

The scene is the single source of truth for "what the page looks like
right now." There is no parallel runtime state tree shadowing an
immutable AST.

### Node

The load-bearing type. Every visible thing — a box, a text run, a panel,
an animation, a live region, the viewport, the status bar — is a node.

```rust
pub struct Node {
    id: NodeId,
    kind: NodeKind,
    parent: Option<NodeId>,
    children: Vec<NodeId>,

    // Placement
    placement: Placement,        // buffer-absolute rect + flow advance
                                 // (post-layout; hit-testing reads directly)

    // Compositing
    buffer: Option<CellBuffer>,  // owned cell buffer; None means children-only
    z_index: i16,
    transform: Transform,        // offset from placement, used for slides/wipes
    visible: bool,

    // Interaction
    focusable: bool,
    hit_target: Option<Action>,  // what happens when clicked/activated

    // Identity for author-facing things
    aml_id: Option<String>,      // e.g. "my-panel" from [panel id="my-panel"]
}
```

There is no `PanelRegion`, `AnimationRegion`, `LiveRegionInfo`, or
standalone `EventBinding`. The features that needed those types are now
node properties.

### Node shape: monomorphic, not sparse

Two shapes were on the table:

- **Monomorphic** — one `Node` struct with all capability fields present;
  `Option<T>` for semantically optional ones.
- **Sparse / ECS** — a small `Node` core plus per-capability slotmaps
  keyed by `NodeId` (`layers: SlotMap<NodeId, LayerData>`,
  `hit_targets: SlotMap<NodeId, Action>`, etc.).

**The decision is monomorphic.** Reasoning:

1. **Scale.** A terminal page has tens to hundreds of nodes. Memory
   cost for unused fields is not measurable; cache behavior of sparse
   maps is not distinguishable from cache behavior of a node slab. No
   performance case for sparse.
2. **Bounded capability set.** The capabilities — placement, buffer,
   z-index/transform, focus, hit-target, aml_id, animation
   binding, live subscription — are fixed by what a terminal display
   needs. This is not a game engine where future capabilities accrete
   unpredictably.
3. **Debugging.** "What is the full state of node N" is one struct to
   read. Dumping a scene as JSON produces a self-contained record per
   node. Sparse requires N lookups across N maps and synthesis to
   answer the same question.
4. **Patch operational simplicity.** `SetTransform { node, value }`
   means `node.transform = value` — a field write. Sparse makes it
   `layers[node].transform = value` — a map lookup plus a field write.
   Small per-patch; large as a cognitive surface when you're reading
   the patch applier.
5. **Invariant locality.** The `Node` struct definition is the entire
   vocabulary of what a node can be. Reading that one type tells you
   everything a node can do. Sparse spreads the vocabulary across
   several type definitions linked by the implicit convention that
   they all key by `NodeId`, which is harder to see and harder to
   enforce.

**What we accept in exchange:**

- Nodes carry `Option`-typed fields they semantically don't use. A
  `NodeKind::Row` has `buffer: None` forever. Fine — an unused
  `Option` is one byte plus padding.
- Adding a new capability means editing the `Node` struct. Given the
  bounded capability set, this is a rare, local change.

**Boundary between `Node` and `NodeKind`.** `Node` holds *generic
scene properties* — placement, compositing, input, identity. These
apply to every node regardless of kind. `NodeKind` holds *what this
node is* along with the data that only makes sense for that kind: a
`Panel`'s list of state children, an `Animation`'s handle to its
runtime, a `LiveRegion`'s subscription. Kind-specific data lives
inside the enum variants, not as more optional fields on `Node`. This
keeps the two concerns separate and keeps `Node`'s field list stable.

### NodeKind

```rust
pub enum NodeKind {
    Root,                        // scene root; holds the page
    Flow,                        // children stack vertically (box, col, document-mode root)
    Row,                         // children stack horizontally
    Absolute,                    // children self-place (screen-mode root, positioned box)
    Text(TextContent),
    Border(BorderStyle),
    Panel { states: Vec<NodeId>, active: NodeId },
    Animation(AnimationHandle),  // WASM or built-in
    LiveRegion(SubscriptionHandle),
    Sticky(StickyAnchor),
    Viewport,                    // the scrollable wrapper
    StatusBar,
    CommandLine,
    Overlay(OverlayData),        // system-synthesized (page transitions, debug, capture)
}
```

`NodeKind` determines layout behavior and how the node reacts to patches.
It does not determine visual appearance — that comes from content and
style.

**`Overlay` is special** among the variants: it has no AML counterpart,
is never produced by `build::from_document`, and is the only kind
constructed via a direct scene method (`Scene::insert_overlay`) rather
than through the parse-build pipeline or `PatchApplier`. Overlays
carry an `OverlaySource` discriminant (`PageTransition`, and future
debug/capture variants) so consumers of a scene dump can identify
what synthesized each overlay. Hit-testing skips them; the layout
pass skips them; the composite walk blits them at Phase D on top of
every other layer so a page transition can cover even foreground
animations on the new page. Buffers are allocated eagerly at
`insert_overlay` time and written by the overlay's owner (e.g.,
`PageTransitionAdapter`) via the kind-gated `overlay_buffer_mut`
accessor.

### Placement

```rust
pub struct Placement {
    rect: Rect,          // parent-space: where this node sits inside its parent
    flow_advance: u16,   // how far the flow cursor moves for the next sibling
    bbox: Rect,          // union of self + children, in parent-space
}

pub struct Rect { x: u16, y: u16, w: u16, h: u16 }
```

`Placement` is the first-class output of layout. It is a property of the
node, not a side-effect of the layout function. Parents compose children's
placements into their own `bbox` and `flow_advance` by rule:

- Flow container: `flow_advance = sum(children.flow_advance)`,
  `bbox = union(children.bbox)`
- Row: `flow_advance = max(children.flow_advance)`,
  `bbox = union(children.bbox)`
- Absolute container: `flow_advance = 0`,
  `bbox = union(children.placement.rect.translated(children.rect.origin))`

`Rect` replaces the three existing `(x, y, w, h)` structs
(`PanelRegion`, `AnimationRegion`, `LiveRegionInfo`) and the
`animation::Rect`.

### Layer

A *layer* is not a separate type. Any node whose `buffer` is `Some` is
a layer. Composition walks the tree bottom-up by z-index, blitting
each layer's buffer onto the parent's compositing surface, honoring
transforms.

The cell model — present/absent cells, topmost-present-wins, no
alpha — is specified in
[04-rendering.md § Cell model](../spec/04-rendering.md#cell-model) and is the
conformance contract for what rendering must produce. This
architecture inherits the rule directly: composition is a cell-by-
cell topmost-present-wins walk, not a multi-pass blend, which is
what makes it cheap. The spec-level guarantee that a foreground
layer's absent cells reveal the background (tooltips over live
content, folded boxes revealing running animations) falls out of
composition without any special path here.

### Patch

The currency of all change.

```rust
pub enum Patch {
    // Structural
    InsertNode { parent: NodeId, before: Option<NodeId>, node: Node },
    RemoveNode { node: NodeId },
    ReplaceChildren { parent: NodeId, children: Vec<NodeId> },

    // Properties
    SetPlacement { node: NodeId, placement: Placement },
    SetTransform { node: NodeId, transform: Transform },
    SetVisible { node: NodeId, visible: bool },
    SetZIndex { node: NodeId, z_index: i16 },

    // Panel / state
    SetPanelActive { panel: NodeId, active: NodeId },

    // Focus / scroll
    SetFocus { node: Option<NodeId> },
    SetScroll { offset: u16 },
}
```

Every source of *structural* scene change produces patches: user input,
animation-emitted property changes, live-region metadata updates,
trigger dispatch. Patches are applied in order by a `PatchApplier` that
mutates the scene's structural state and updates the invalidation set.

Notice what is *not* in the `Patch` enum: per-cell buffer writes.
Buffer contents are produced by the subsystem that owns the buffer (see
Invariant 3b below) and written directly; the change is signaled
through the dirty set, not through the patch stream. Patches describe
structural changes: a buffer *exists*, is *placed*, is *visible*, is
*transformed*, but not what's *inside* it.

This is the central discipline of the design. Structural changes are
observable, replayable, and diff-able through patches. Content changes
are bounded by buffer ownership and summarized per tick through the
dirty set. Neither channel pretends to be the other.

### Invalidation

Each patch invalidates some subset of nodes (for re-layout) and/or some
subset of screen rectangles (for re-composition). The render loop reads
these sets, does exactly that work, clears them, and proceeds.

```rust
pub struct Invalidation {
    layout: NodeIdSet,       // subtrees needing layout
    composite: DirtyRegions, // screen-space rects needing repaint
    present: DirtyRegions,   // rects needing ANSI emission
}
```

`composite` is a superset of `present`: a node moving changes composition
at both its old and new positions, but only the union needs ANSI output.

## AST to Scene mapping

The scene is built from a parsed, component-expanded `Document` by a
pure function `build_scene(doc) -> Scene`. The mapping is not a naive
element-per-node duplication — some AST elements do not render at all
(metadata), and some do not become separate nodes (inline styled
spans). This section specifies the contract precisely enough for the
parity assertion to check it.

### Three categories of AST element

Every `Element` variant falls into exactly one of three categories:

1. **Node-bearing.** Produces exactly one scene node. Its children are
   recursively mapped.
2. **Ancillary.** Produces zero scene nodes. Its information is
   consumed during scene construction and attached elsewhere
   (document-level metadata, style defaults).
3. **Inline.** Produces zero scene nodes. Its information becomes part
   of the content of an ancestor node (styled spans inside text).

### The mapping table

This table is definitive. Adding a new AST variant requires extending
it; the parity assertion depends on its completeness.

| AST element | Category | Scene NodeKind (if node-bearing) |
|---|---|---|
| `Box` with `x.is_none() && y.is_none()` | node-bearing | `Flow` |
| `Box` with `x.is_some() || y.is_some()` | node-bearing | `Absolute` |
| `Row` | node-bearing | `Row` |
| `Col` | node-bearing | `Flow` (vertical) |
| `Header`, `Body`, `Footer`, `Nav` | node-bearing | `Flow` |
| `Hr`, `Spacer` | node-bearing | `Flow` (leaf, pre-populated buffer) |
| `Text` | node-bearing | `Text` |
| `Pre` | node-bearing | `Text` (preserve-whitespace) |
| `Heading` | node-bearing | `Text` (heading level) |
| `List`, `Item` | node-bearing | `Flow` |
| `Link` | node-bearing | `Text` with `hit_target: Some(Navigate)` |
| `Input`, `Select`, `Button` | node-bearing | `Input` / `Select` / `Button` |
| `Option` | node-bearing | leaf child of `Select` |
| `Form` | node-bearing | `Flow` (groups its inputs for submit) |
| `Panel` | node-bearing | `Panel { states: Vec<NodeId>, active: NodeId }` |
| `State` (child of Panel) | node-bearing | `Flow` (state-root; lives in `Panel.states`) |
| `Details` | node-bearing | `Flow` (collapsible) |
| `Table`, `Tr`, `Th`, `Td` | node-bearing | `Table` / `Tr` / `Th` / `Td` |
| `Live` | node-bearing | `LiveRegion(SubscriptionHandle)` |
| `Animation` (frame/tween/wasm) | node-bearing | `Animation(AnimationHandle)` |
| `Meta` | ancillary | (becomes `Document.meta` on the scene root) |
| `Style` | ancillary | (becomes `Scene.style_defaults`) |
| `Def` | ancillary | (consumed at parse time; never reaches `build_scene`) |

### Inline content inside `Text`

A `Text` element's `children: Vec<Element>` holds inline runs —
typically other `Text` elements carrying style attributes (bold,
italic, color spans). These children are **inline**: they become
styled run data inside the parent `Text` node's `content`, not
separate scene nodes.

Specifically:

- `[text]hello [b]world[/b][/text]` produces **one** `Text` node whose
  `content` is a run list: `[{text: "hello ", style: default},
  {text: "world", style: bold}]`.
- `[text]outer [text fg="red"]inner[/text] tail[/text]` similarly
  produces **one** `Text` node with three runs (default, red, default).
- A `[link]` inside a `[text]` is still node-bearing (it has a
  `hit_target`). So `[text]click [link href="..."]here[/link][/text]`
  produces **two** nodes: the `Text` node and the `Link` node as its
  child. The link's text content is, in turn, one `Text` node under
  the `Link`.

This rule is applied recursively during scene construction: when
walking a `Text` element's children, each child is classified per the
table; node-bearing children become child nodes, inline children
become runs in the parent's content.

### The isomorphism, formally

Let `Nb(e)` be the predicate "element `e` is node-bearing." Let
`scene_children(e)` be `e.children.iter().filter(|c| Nb(c))` in source
order.

Then `build_scene` satisfies:

1. **Bijection.** There is a bijection `f: Nb-elements-of-doc →
   nodes-of-scene` such that `f(doc.page.body) = scene.root`.
2. **Parent relationship.** For node-bearing `e` and its node-bearing
   child `c`: `f(c).parent = Some(f(e).id)`.
3. **Sibling order.** For node-bearing `e`: `f(e).children` is
   `scene_children(e).map(f)`, in the same order.
4. **Kind preservation.** `f(e).kind` is given by the mapping table,
   specialized on attributes where the table distinguishes them
   (e.g., `Box` with/without `x`/`y`).
5. **Identity preservation.** If `e` has an AML `id` attribute, then
   `f(e).aml_id = Some(e.id)`. Otherwise `f(e).aml_id = None`.
6. **Ancillary absence.** No ancillary element has a corresponding
   node in the scene. (`Meta`, `Style`, `Def` are absent.)
7. **Inline absence.** No inline-classified child of a `Text`-family
   element has a corresponding node in the scene; it is represented
   as a styled run in the parent's content.
8. **Component expansion precondition.** `build_scene` assumes the
   parser has already expanded all component usages. Any `Def` or
   `Use` element reaching `build_scene` is a bug.

These eight properties are what a parity check actually checks. "Tree
structure matches the AST" is shorthand for this; the shorthand is
what is asserted, but the definition of "matches" is this list.

## The Render Loop

The spec-level per-tick stage ordering and guarantees are defined in
[04-rendering.md § The tick](../spec/04-rendering.md#the-tick). This section
describes how those stages map onto this implementation.

Once per tick (~33ms):

```
  1. drain input events        → patches (focus, trigger, scroll, navigate)
  2. advance time              → patches (from active animations)
  3. drain protocol events     → patches (live-region updates, deltas)
  4. apply patches             → updated scene, populated invalidation set
  5. layout pass               → walk invalid subtrees, update placements and buffers
  6. composite pass            → repaint dirty screen regions from layer stack
  7. present pass              → emit ANSI for dirty rectangles
  8. sleep to tick boundary
```

The mapping from spec to implementation: spec stages 1–3 (user
input, time advancement, protocol events) land as this doc's
stages 1–3, each producing patches that queue into the scene.
Spec stage 4 (layout) is split here into patch application (4)
and scene-native layout (5) — the architecture-specific split
between mutation bookkeeping and layout computation. Spec stage 5
is this doc's stage 6; spec stage 6 is this doc's stage 7. The
spec permits this kind of decomposition explicitly so long as the
order of visible effects is preserved.

Each stage processes only what is dirty. An idle page does zero work past
stage 1. An animated page does work proportional to the animation's
region. A state change does work proportional to the changed subtree.

The public entry points and FIFO reducer dispatcher remain in `terminal.rs`.
That module owns a sealed `ReducerPort`; runtime modules can read model state
and dispatch domain events but cannot borrow or mutate `ViewerModel` directly.
`terminal/runner.rs` owns `TerminalRuntime`, whose single mutating effect API
returns domain completion events without mutating the model. Per-tick input,
animation, live polling, layout, composition, and presentation converge there
in a fixed order. Projection types live in `terminal/presentation.rs`, terminal
restoration errors in `terminal/lifecycle.rs`, and drawing helpers in
`terminal/rendering.rs`. `TerminalRuntime::execute` owns fetch, submit,
redirect, parse, WASM-resource preparation, layout, and cached-history effect
execution. It buffers every intermediate artifact by exact `OperationOwner`;
only `ActivateLayout` may replace the active page. `terminal/navigation.rs`
now contains focused recovery coverage rather than a parallel production
transport path. Remaining RC work is transactional resource governance and
the platform verification gates, not a presentation-ownership exception.
Resize relayout, animation ticks, deferred navigation, subscription retirement,
scoped input/focus projection, authored events, animation paints/patches,
panel/details relayout, transition capture/install, and final rendering are
reducer-issued runtime effects as well. The loop may poll and classify ingress,
but an architecture test rejects direct active-scene mutation there.
Prepared WASM bytes remain attached to the loaded page for animation-topology
rebuilds, so panel/details relayout never performs a hidden network request or
dispatches reducer events from the animation module.

Compositor budget rejection follows the same FIFO boundary. A reducer-issued
effect first evicts one LRU resource and authorizes a render retry; if that
cannot free capacity, a second effect removes the oldest non-current logical
history entry and matching presentation artifact before another retry. Current
history is protected. Exhausting both tiers is the only path that retires page
work and installs the independently governed client error page.

## Scene state vs. subsystem state

Before listing invariants, we have to be precise about what the scene
*is*. A single claim that "all scene changes go through patches" is
not honest for a system where a WASM animation writes thousands of
cells per tick and a tween's `t` value advances every frame.
Patch-encoding that volume is bookkeeping without benefit; not
patch-encoding it and claiming the invariant is a lie.

We draw the line as follows.

**Scene state** — the observable state of what is on screen:

- The node tree: which nodes exist, their kinds, their parent/child
  relationships.
- Per-node compositing properties: placement, z-index, transform,
  visible, focusable.
- Per-node identity: `aml_id`, `hit_target`.
- Panel active state, current focus, scroll offset.
- **Node buffer contents** are scene state in the sense that
  composition reads them, but their per-cell updates are not
  patch-mediated (see below).

**Subsystem state** — state that drives the scene but is not
directly observed:

- Animation progress: `t` values, frame indices, elapsed time
  counters, started-at timestamps.
- WASM VM state: linear memory, stack, fuel remaining for the tick.
- Live-region network buffers, incoming deltas waiting to be applied.
- Input event queues.

Scene state is patch-mediated. Subsystem state is owned by whichever
subsystem produces it and mutated freely. The boundary between them
is the **patch** (for structural scene changes, applied via
`PatchApplier`) and the **dirty-set notification** (for buffer content
changes, posted by subsystems directly into `Invalidation.composite`).
These are the two channels described throughout this document; any
reference to "the dirty set" means `Invalidation.composite` specifically.

## Invariants

The architecture enforces these properties. They are what make it correct.

1. **Node identity is stable.** A `NodeId` refers to the same node across
   ticks until a `RemoveNode` patch removes it.

2. **(a) Structural scene changes go through patches.** Adding or
   removing nodes, reparenting, changing placement, transform,
   visibility, z-index, focus, scroll, or panel active state — all
   happen via `Patch` applied by `PatchApplier`. There is no other path.
   This is enforced by making the mutating methods on `Scene` and
   `Node` private to the scene module, accessible only through
   `PatchApplier`.

3. **(b) Node-owned buffers are written only by the node's owning
   subsystem.** Every node whose `buffer` is `Some` has exactly one
   subsystem responsible for producing its cells:

   | Node kind | Owning subsystem |
   |---|---|
   | Flow, Row, Absolute (containers with buffers) | Layout |
   | Text, Border | Layout |
   | Panel (holds the composed active state) | Composition |
   | Animation (frame, tween, effect) | The animation kind |
   | Animation (WASM) | The WASM runtime |
   | LiveRegion | The live-region subsystem |

   Cross-subsystem buffer writes do not happen. When a subsystem
   finishes its per-tick writes to a buffer, it emits a single
   `CompositeInvalidated { node: NodeId }` notification into the
   dirty set — not a `WriteCells` patch per cell.

   The practical implication: during the "advance time" stage of the
   render loop, the WASM runtime is handed mutable access to its
   owning node's buffer. It calls `set_cell` via its host function as
   many times as its module wishes (within fuel). At tick end, one
   dirty-set notification is posted. External observers see a
   consistent buffer at composition time; they do not observe (or
   need to observe) the individual writes.

4. **Placement is derived during layout.** `layout_scene` writes
   each node's `placement.rect` in buffer-absolute coordinates.
   Hit-testing, focus rect queries, and animation region lookups
   read `placement.rect` directly; there is no separate screen-space
   cache today because the composite walk does not produce one.

5. **Layers own their cells.** A direct consequence of 3(b): no layer
   writes into another layer's buffer. Overlaps are resolved at
   composition time, not at write time.

6. **Composition order is deterministic.** Children composite in
   z-index order; ties by insertion order.

7. **Patches are applied in arrival order within a tick.** Order
   between ticks is undefined for patches produced in parallel (e.g.,
   two animations advancing simultaneously); each tick is a
   serialization point.

8. **Layout is pure over its subtree.** `layout(node, parent_frame)`
   has no effect outside the subtree rooted at `node`. Global state
   does not exist at layout time.

9. **The scene is serializable at tick boundaries.** Between tick
   processing stages, the scene can be dumped to a debug-readable form
   (text tree or JSON) without losing scene state. Subsystem state is
   not included; that lives with its subsystem. This is the test that
   the scene state is not hiding anywhere else — not that subsystem
   state is.

### Why split 2 into 2a and 2b

These are two different guarantees with two different enforcement
mechanisms.

**2a (patches for structure)** is enforced by visibility: the compiler
prevents any code outside `PatchApplier` from mutating a `Node`'s
fields. If you try, it doesn't compile.

**2b (one subsystem per buffer)** is enforced by the scene's type
system at a higher level: when a node is constructed with a buffer,
the buffer is bound to its owning subsystem by the node's `kind`. The
buffer is exposed only through methods specific to that subsystem
(e.g., `Scene::wasm_buffer_mut(node: NodeId) -> Option<&mut
CellBuffer>` returns `Some` only for `NodeKind::Animation(Wasm(_))`).
Cross-subsystem access requires casting away the kind, which is a
deliberate, auditable act.

Together they give us what the original Invariant #2 tried to promise
(a single path for observable changes) without pretending that 57,600
per-second cell writes want to be patches. The scene is observable at
tick boundaries; that is the only observation anyone needs to make.

### Enforcement sketch: field-level write authority

"Node fields are private" is too blunt. The honest rule is per-field,
with each field declaring its authorized writers. The following table
is the contract.

| Field | Category | Authorized writers |
|---|---|---|
| `id` | Identity (immutable after construction) | `scene::build` only, at node creation |
| `kind` (enum shell) | Identity (immutable after construction) | `scene::build` only |
| `kind.Panel.active` | Structural | `scene::patch::PatchApplier` via `SetPanelActive` |
| `parent` | Structural | `scene::patch::PatchApplier` via structural patches |
| `children` | Structural | `scene::patch::PatchApplier` via structural patches |
| `placement` | Layout output | `scene::patch::PatchApplier` (via `SetPlacement`) **and** `layout` pass (via `Scene::update_placement`) |
| `z_index` | Property | `scene::patch::PatchApplier` via `SetZIndex` |
| `transform` | Property | `scene::patch::PatchApplier` via `SetTransform` |
| `visible` | Property | `scene::patch::PatchApplier` via `SetVisible` |
| `focusable` | Immutable after construction | `scene::build` only |
| `hit_target` | Immutable after construction | `scene::build` only |
| `aml_id` | Immutable after construction | `scene::build` only |
| `buffer` (Option itself) | Structural | `scene::build` only (sets `Some`/`None`) |
| *Contents of `buffer`* (the `CellBuffer`) | Subsystem content | Owning subsystem via `Scene::{kind}_buffer_mut` |

### How Rust enforces it

The following visibility scheme gets the compiler to do the work:

```rust
// crates/dustnet-client/src/compositor/scene/node.rs
pub struct Node {
    pub(super) id: NodeId,
    pub(super) kind: NodeKind,
    pub(super) parent: Option<NodeId>,
    pub(super) children: Vec<NodeId>,
    pub(super) placement: Placement,
    pub(super) z_index: i16,
    pub(super) transform: Transform,
    pub(super) visible: bool,
    pub(super) focusable: bool,
    pub(super) hit_target: Option<Action>,
    pub(super) aml_id: Option<String>,
    pub(super) buffer: Option<CellBuffer>,
}

impl Node {
    // Public read-only accessors for every field. No &mut accessors.
    pub fn id(&self) -> NodeId { self.id }
    pub fn kind(&self) -> &NodeKind { &self.kind }
    pub fn placement(&self) -> &Placement { &self.placement }
    // ... etc for every field.
    pub fn buffer(&self) -> Option<&CellBuffer> { self.buffer.as_ref() }
}
```

`pub(super)` makes each field visible only within the scene module
(`crates/dustnet-client/src/compositor/scene/`). Direct writes from outside the module fail
to compile.

Authorized writers live inside the scene module:

- `scene::build` — constructs nodes, writing initial values once.
- `scene::patch` (i.e. `PatchApplier`) — writes structural and
  property fields in response to patches.

Pass outputs that don't fit either category (layout writing
`placement`) are written via narrowly-scoped `pub(crate)` methods
on `Scene`:

```rust
// crates/dustnet-client/src/compositor/scene/mod.rs
impl Scene {
    pub(crate) fn update_placement(&mut self, n: NodeId, p: Placement) {
        if let Some(node) = self.nodes.get_mut(n) {
            node.placement = p;
        }
    }
}
```

Called only by `compositor::layout`. Any other caller is an
architectural violation and shows up trivially in code review
(`rg update_placement` must only match in its authorized callers).

Buffer content writes use kind-gated public methods that return `&mut
CellBuffer` only if the caller matches the node's subsystem:

```rust
impl Scene {
    pub fn layout_buffer_mut(&mut self, n: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(n)?;
        match &node.kind {
            NodeKind::Flow | NodeKind::Row | NodeKind::Absolute
            | NodeKind::Text(_) | NodeKind::Border(_) => node.buffer.as_mut(),
            _ => None,
        }
    }
    pub fn wasm_buffer_mut(&mut self, n: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(n)?;
        match &node.kind {
            NodeKind::Animation(AnimationHandle::Wasm(_)) => node.buffer.as_mut(),
            _ => None,
        }
    }
    pub fn live_buffer_mut(&mut self, n: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(n)?;
        match &node.kind {
            NodeKind::LiveRegion(_) => node.buffer.as_mut(),
            _ => None,
        }
    }
}
```

A subsystem calling the wrong accessor gets `None` — it cannot write
to a buffer whose owning subsystem is different. Adding a new
subsystem means adding a new `*_buffer_mut` method with its own kind
gate; the existing gates continue to reject unauthorized access.

### Reading vs. writing

Every field has a public read accessor; no field has a public mutable
accessor. Reads are unrestricted by design (any code that needs to
layout, composite, present, hit-test, or debug-dump the scene reads
widely). Writes are tightly scoped, and that's the asymmetry that
makes the invariants enforceable without sacrificing ergonomics.

## Common Operations

### Building the scene from AML

```
Document (from parser)
  → build_scene(doc) -> Scene
  → for each AST element, create a Node; recurse into children
  → resolve [def]-expanded components (already done by parser)
  → register animations and subscriptions as Animation and LiveRegion nodes
  → run initial layout pass over the whole tree
  → run initial composite pass over the full viewport
  → emit initial ANSI
```

This happens once per navigation. After this point, the scene is
mutated by patches.

### A panel state change

User presses Enter on a button bound to `set-panel panel=my-panel state=b`:

```
  input → Action::SetPanelState { panel: my-panel, state: "b" }
        → patch: SetPanelActive { panel: node_of("my-panel"), active: node_of("b") }
  apply → node's active child changes; its subtree is invalidated for layout
  layout → re-lays out only the panel node's subtree; sibling and ancestor
           layouts are untouched
  composite → repaints only the panel's screen rect
  present → emits ANSI for that rect
```

Everywhere else on the page — scroll position, focus, other animations,
live-region content — is untouched because nothing told it to change.

### An animation tick

A slide animation moves node N's transform from `(0, 0)` to
`(20, 0)` over 500ms. At t=0.333 (167ms in):

```
  advance time → tween internally advances its state: t=0.333
                 (this mutation is subsystem state — no patch involved)
               → tween computes its scene-visible effect and emits:
                 SetTransform { node: N, transform: Transform { x: 7, y: 0 } }
  apply → node N's transform updated (Patch applied to Scene via PatchApplier);
          node N's screen rect marked for re-composition at both the old
          and new positions (not re-layout — transform is a composite property)
  composite → composites layer N at its new translated position;
              cells at the old position show whatever is underneath
  present → emits ANSI for the union of N's old and new rects
```

The distinction matters: the tween's `t` is subsystem state (owned by
the animation object, mutated freely). The node's `transform` is scene
state (owned by the scene, mutated only through `PatchApplier`). The
animation's `advance()` method bridges the two — reads its own state,
emits patches describing the scene-visible effect.

If two animations target the same node's transform, their patches
apply in arrival order within the tick; typically the animation system
disallows conflicting animations on the same property for the same
node.

### A scroll

User presses PageDown:

```
  input → Action::Scroll { delta: +viewport_height }
        → patch: SetScroll { offset: new_offset }
  apply → viewport's scroll state updated; viewport's screen rect marked
          for re-composition
  composite → viewport re-reads its buffer at the new offset, composites
              sticky headers on top per Sticky node rules
  present → emits ANSI for the viewport rect
```

No layout runs. The buffer is already fully rendered; scrolling is
purely a viewport-level composition change.

### A transition between panel states (dissolve)

State A is a box with matrix-rain animation inside it; state B is a
3-line text box. The panel's state-B transition is `dissolve` over 400ms.
User triggers B:

```
  input → patch: SetPanelActive { panel, active: B }
  transition scheduled → DissolveAnimation over 400ms; keeps both A's
                         and B's layer buffers alive for the duration
  tick 1 → dissolve animation advances t=0.08; picks a per-cell mask:
           "which cells have flipped from A to B so far"
         → writes the composed cells into the panel's own buffer,
           sourcing each cell from A or B per the mask
         → posts CompositeInvalidated { panel_node } to dirty set
           (no patches — the panel buffer is the panel's scene buffer;
            the dissolve owns its writes for the transition's duration)
  composite → composites the panel layer (already resolved) onto the base
  present → emits ANSI for the panel region
  ...
  tick N (400ms in) → mask is fully B; the dissolve finishes
                    → retires: A's layer dropped, B takes over as the
                      panel's active-state layer, B's own content
                      production resumes (layout/animation writes B's
                      buffer normally from this tick on)
```

A's matrix-rain animation continues to *write into A's layer buffer*
throughout the dissolve — the dissolve reads A's current cells, not a
frozen snapshot. So the fading rain is live rain, right up until each
of its cells gets flipped to B.

Shape-mismatched states (A is 10×5 at top-left, B is 3×3 at
bottom-right) work by the same mechanism: cells in A but not in B stay
showing A until they flip to B (or reveal the base layer if B has no
content there). Cells in B but not in A similarly fade in per the mask.
No alpha is required; each cell at each instant is sourced from exactly
one of A, B, or the underlying layer.

Wipe and slide use the same kind of scheduling (keep both layers alive
for the duration) but with different per-cell decision functions:
`wipe` chooses by the current boundary x-coordinate; `slide` chooses by
each layer's translated position within the region.

### A stacked animation

The page has a matrix-rain animation at `z=0` covering the full canvas,
and a modal dialog at `z=10`. The dialog's buffer contains opaque cells
where its border, title, and body text are, and absent cells everywhere
else (its bg-fill region is opaque; areas truly outside the dialog's
rect are not composited at all).

```
  tick → WASM runtime ticks the rain animation:
         - given mutable access to the rain node's buffer
         - makes its set_cell calls (within fuel budget)
         - on return, posts CompositeInvalidated { rain_node } to dirty set
       → dialog has no active animation; emits nothing
  dirty set → { composite: rain_node's screen rect }
              (no patches, no layout invalidation)
  layout → skipped; nothing layout-invalid
  composite → for each cell in the dirty region, walk layers top-down
              by z-index until a present cell is found:
              - check dialog layer at (x, y): present? use it : continue
              - check rain layer at (x, y): present? use it : continue
              - fall back to base layer / blank
  present → emits ANSI for the dirty region
```

The matrix rain wrote thousands of cells this tick. Zero patches were
produced. Zero bookkeeping was wasted. The scene is consistent at the
tick boundary — the rain layer's buffer contents reflect the tick's
writes, and composition reads them without caring how they got there.

The dialog is "seen through" only by cell presence: wherever the dialog
has no cell, the rain shows. This is the terminal-native version of
what other systems use alpha for, and it costs nothing beyond
"topmost-present wins."

The rain layer and the dialog layer are independent; neither knows about
the other. Composition handles the combination. The rain keeps ticking
whether or not the dialog is visible; if the dialog is dismissed, the
rain is already running and simply becomes the topmost layer again.

### A box folding out, revealing a running animation beneath

The page has matrix-rain at `z=0` and a foreground box at `z=1` whose
height is animated from full to 0 (fold out) and back:

```
  tick k → fold animation emits SetPlacement { box, new_rect with h=...}
  apply → box's placement updated; its subtree is re-laid-out (its
          children re-fit); its screen rect and the rect it *used* to
          occupy are both marked dirty for composition
  layout → only the box's subtree
  composite → for every dirty cell:
              - if the cell is now within the box's smaller rect: show box
              - if the cell is no longer within the box (newly revealed):
                show whatever is underneath → the rain layer
  present → emits ANSI for the dirty region
```

The rain animation was never paused; it is visible wherever the box is
not. This is the key capability that falls out of the architecture: a
layer does not need to know about layers above or below it to be
correctly revealed or occluded.

## Transitions

The five transition kinds (`Cut`, `Wipe`, `Slide`, `Dissolve`,
`Scale`) and their per-cell decision functions are specified in
[04-rendering.md § Transitions](../spec/04-rendering.md#transitions). That
is the authored contract; all conforming clients must implement it.

Architecturally, this implementation realizes transitions by keeping
both the outgoing and incoming layers alive as scene nodes for the
transition duration, and sourcing each output cell from exactly one
of them per tick via a kind-specific decision function. No alpha
path, no blend buffer — the cell-presence rule from the spec is
sufficient, which is what makes this architecture cheap.
`TransitionAdapter` in `crates/dustnet-client/src/compositor/animate/transition.rs`
implements the four non-`Cut` kinds for panel-level transitions
(`Cut` is born finished). Page-level transitions between navigated
pages work the same way via `PageTransitionAdapter` in
`crates/dustnet-client/src/compositor/animate/page_transition.rs` — the adapter paints its
blended frame into a system-synthesized `NodeKind::Overlay` node at
`z = i16::MAX`, and the composite walk's Phase D blits it above
every other layer. On finish the adapter emits `Patch::RemoveNode`
to drop the overlay. Both adapters share the pure
`render_transition_frame` blending function.

## Implementation notes

The sections above describe the model; these are the facts that follow from
it in `crates/dustnet-client/src/compositor/`.

**Buffer ownership.** Every visible `NodeKind` owns its own `CellBuffer` and
writes at local coordinates. `composite::walk` iterates buffered scene nodes
in tree order, blitting each at its global `placement.rect` and honoring
per-node `z_index` and `Transform`. `LayoutResult.buffer` / `page.buf` is
retained only as a dimensions holder for `Compositor` sizing and the viewport
scroll-metrics paths; no cells are written into it. `Compositor` itself is a
thin dimension-holding handle rather than a layer store.

**Invalidation.** `Compositor` retains the previous tick's composited
`CellBuffer` and short-circuits — returning the cached frame without walking
the scene — whenever `scene.invalidation.composite` is empty at composite
time. The cache is invalidated by `resize` (dimensions differ) and by any
`mark_composite` between ticks; an animation tick's `wrote_buffers` feed
`mark_composite` explicitly, so direct buffer writes cannot bypass
invalidation. `layout_pass_invalidated` drains only the layout set; composite
and present are cleared after the present pass consumes them. Idle pages skip
the scene walk entirely; active pages walk only when invalidation says they
must.

**Presentation.** `Compositor::present_main` diffs the current frame against
the previously-presented one and emits only changed cells via `render_diff`.
It falls back to `render_full` on the first frame, after a resize, after a
viewport-offset change, and after `invalidate_presented` — which is retained
for alt-screen switches and an external `clear`, and is not called by page
transitions. An idle tick therefore produces **zero bytes on the wire**.

**Page transitions.** `PageTransitionAdapter` (`animate/page_transition.rs`)
implements the `Animation` trait and paints its blended frame into a
`NodeKind::Overlay` node's buffer each tick. The walk picks it up at Phase D,
above every other scene layer, so a running transition occludes foreground
animations on the incoming page. When the adapter finishes it emits
`Patch::RemoveNode` to drop the overlay. Keeping transitions inside the walk
is what lets `render_diff` stay continuous across the transition boundary:
the first post-transition frame diffs against the last transition frame
rather than forcing a full repaint. `render_transition_frame` remains
`pub(crate)` in the animate module so other compositor paths can call it —
debug capture, for instance — without a back-edge.

**Terminal integration boundary.** `terminal.rs` is the ordered reducer
integration point. The concrete `TerminalRuntime` in `terminal/runner.rs`
owns transport and projection state and executes reducer effects; reconnect
replay, subscriptions, buffered live updates, resize/timer activation,
pressure recovery and shutdown all return their completions through the FIFO
dispatcher. Reducer-owned network and cached navigation execute through
`TerminalRuntime::execute`; prepared page results are stored by exact owner
and installed only by a matching reducer-issued `ActivateLayout`. Stale
prepared pages are dropped without mutating the active scene.
`terminal/navigation.rs` retains focused recovery tests rather than a
parallel production path.

**Settled policies.** Buffer allocation is per-node, as above. Finished
content animations remain on their final frame, while transient
page-transition overlays remove themselves through a scene patch. Input is
handled before the next animation advancement, so a structural user action
may skip a visual frame. Layout caching across ticks — reusing a subtree's
placement when its inputs have not changed — is a performance optimization
deliberately left undone until a profile shows it matters.

## Module Layout

The implementation currently has this package-owned shape (test-only modules
are omitted):

```
crates/dustnet-client/src/compositor/
  scene/
    node.rs             Node, NodeId, NodeKind, Placement, Rect, Transform
    tree.rs             Scene, child iteration, parent/ancestor walks
    patch.rs            Patch enum, PatchApplier, invalidation population
    build.rs            AST -> Scene initial construction
    events.rs           event bindings and dispatch metadata
    input.rs            focus, hit testing, and actions
    invalidation.rs     dirty-region tracking
  layout/
    engine.rs           scene layout and buffer construction
    kinds/              element-kind layout routines
    text.rs             grapheme measurement, wrapping
    border.rs           border painting into a node's own buffer
    cell.rs             styled cells and governed cell buffers
    rect.rs             layout rectangles
  animate/
    mod.rs              Animation trait, time advancement, patch emission
    runtime.rs          active animation collection and ticking
    tween.rs            property interpolation
    frame.rs            keyframe sequences
    effect.rs           text effects (typewriter, decode, rainbow) as Animations
    transition.rs       panel-state transitions
    page_transition.rs  navigated-page transitions
    wasm.rs             WASM animation adapter
  composite.rs          composition walk and flat-buffer compatibility adapter
  panels.rs             transactional panel state changes and snapshots
  present/
    mod.rs              dirty-rect ANSI emission; style-diffing within a frame
    ansi.rs             stateless SGR and cursor sequences
    probe.rs            ambiguous-width probe
  terminal.rs           public viewer façade and sole reducer dispatcher
  terminal/
    runner.rs           ordered terminal/transport effect execution loop
    presentation.rs     presentation ownership boundary
    lifecycle.rs        shutdown/restoration ownership boundary
    navigation.rs       reducer-owned network and cached navigation execution
    events.rs           authored-event scheduling and deferred navigation gate
    rendering.rs        status/command-line rendering and safe text helpers
  terminal_lifecycle.rs terminal setup and restoration guards
  wasm.rs               sandboxed WASM runtime host
```

Workspace ownership around the compositor:

```
crates/dustnet-core/src/       ATP/AML values, codecs, scanner, parser, protocol state
crates/dustnet-client/src/     client transport, sessions, viewer, compositor, config
crates/dustnet-server/src/     StaticServer and server transport
crates/dustnet/src/            dustnet browser/authoring CLI
crates/dustnetd/src/           dustnetd static-server CLI
```

The workspace has no compatibility facade, and no authentication or plugin
experiment: neither is part of the production server architecture.

## What This Enables

The model was never about parity with what it replaced. It unlocks
capabilities the earlier system could not express without special
casing:

- **Stacked independent animations with reveal-through-gaps.** A
  dialog over a running animation background: the dialog's opaque
  cells cover the rain; its absent cells reveal it. Both layers tick
  independently; neither knows about the other.
- **Animations that persist across panel state changes.** A ticker bar
  at the top of the page does not reset when a panel below it changes
  state; only the panel's subtree is invalidated.
- **Shape-mismatched transitions.** State A occupies `10×5` at the top
  left; state B occupies `3×3` at the bottom right. Dissolve, wipe,
  slide all work: the base layer shows through cells owned by neither
  state at any given instant.
- **Parallax scroll.** Background layer moves slower than foreground by
  having a different transform function over scroll offset.
- **Hover/focus overlays over live content.** Tooltip layer appears
  without freezing the live region beneath. Wherever the tooltip has
  no cells, the live region shows.
- **Reveal on fold/collapse.** A box folding out exposes whatever was
  underneath — a running animation, another panel, the base layer —
  purely through presence/absence as the box's cells retract.
- **Cheap live regions.** Updates touch only the live region's node;
  everything else is cached. Scroll and unrelated animations continue
  at full frame rate regardless of how noisy the live region is.
- **Delta-driven server updates.** The server's delta protocol (the
  `f271595` commit) maps naturally onto scene patches: the server
  streams `Patch` values, the client applies them. No separate delta
  engine needed.
- **Hit-testing, scroll-into-view, screenreader landmarks.** These all
  need to know "where is node X on screen" — which is the property
  every node has, trivially.

## What this architecture does not cover

Boundaries — things the scene-graph compositor is deliberately not
doing, either because they belong to a different subsystem or
because they are excluded by the spec:

- **AML surface language, ATP wire protocol, scanner, parser, color
  model, escape sanitization.** Unchanged by the compositor
  redesign; owned by their respective modules and docs.
- **Spec-level non-goals** (no alpha, no 60fps, no browser APIs, no
  3D, no partial initial page streaming) are specified in
  [04-rendering.md § Non-goals](../spec/04-rendering.md#non-goals). They
  are not architecture choices — a sibling implementation is bound
  by them too.

Architecture-level design exclusions specific to this
implementation:

- **Not a retained-mode GUI framework.** No signals/reactive values,
  no component lifecycle, no virtual-DOM diffing. Patches come from
  a small set of well-defined sources (input, time, protocol), not
  from a dependency graph.
- **Not a game engine.** No physics, no scene-graph transforms
  beyond 2D offsets. Transforms are strictly what the spec allows
  (position offset, clip rect).
- **Not a general-purpose terminal multiplexer.** Does not host
  other processes, tmux-style panes, or PTY streams. The only
  things painting into the cell grid are AML-declared content and
  sandboxed WASM animations.

### WASM integration note

The author-visible WASM contract (six host functions, region-
relative coordinates, fuel budget, 1 MiB memory cap) is specified
in [04-rendering.md § WASM animations](../spec/04-rendering.md#wasm-animations).
This implementation's only architecturally interesting choice is
that the scene node's own buffer is lent to the WASM runtime via
`mem::swap` in `WasmAnimationAdapter` for the duration of each
tick, so composition reads the module's writes in place instead of
post-copying from an internal scratch buffer. This is a plumbing
detail — a WASM module compiled against the spec runs unmodified
either way.

## Glossary

- **Scene** — the persistent display state; a tree of Nodes.
- **Node** — a single element in the scene; has identity, placement,
  optional buffer, optional children.
- **Layer** — a node whose `buffer` is `Some`; composited into its
  parent.
- **Placement** — a node's rect in parent-space plus flow advance.
- **Patch** — a declarative mutation applied to the scene.
- **Invalidation** — the set of nodes/regions that need re-layout or
  re-composition after a batch of patches.
- **Composition** — the bottom-up walk that blits layer buffers into
  the output.
- **Present** — the final step that emits ANSI for dirty rectangles.
- **Tick** — one iteration of the render loop (~33ms).

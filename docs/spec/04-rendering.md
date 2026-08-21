# Rendering — Display Model

## Overview

This document specifies how AML is rendered to a terminal — what a
conforming Dustnet client must produce on screen given a loaded document,
user input, and protocol updates over time. It is implementation-
agnostic: any conforming client, in any language or runtime, must
produce the behavior described here. The reference implementation's
internal structure is covered separately in
[compositor.md](../internals/compositor.md).

Rendering is not a stateless transform from AML to ANSI bytes. A loaded
document establishes a **persistent display state** — a scene of
positioned content — which user input, animation, and server-pushed
deltas mutate over time. The client composites this state to the
terminal on a periodic tick.

## Cell model

A terminal cell carries one glyph, one foreground color, one
background color, and zero or more **style flags** (see the Style
section below for the exhaustive list). Every cell is in exactly
one of two states:

- **Present** — the cell carries content that is drawn.
- **Absent** — the cell carries no content; the layer below shows
  through.

There is no partial blending, no alpha channel, no per-cell opacity.
Two layers at the same position resolve by presence alone: the
topmost present cell wins; absent cells pass through to the layer
below. "Transparency" in Dustnet rendering means cell-absence, not alpha.

### When is a cell present?

A cell is **present** when either of the following is true:

- It contains a non-space glyph, *or*
- It contains a space character **with an explicit background
  color**.

A cell is **absent** when it contains a space character with no
explicit background color and no explicit foreground styling —
the default-styled empty cell.

This distinction — known as the **opaque-space rule** — is
load-bearing. Filled backgrounds, bordered boxes with painted
interiors, and masked dissolves all rely on "space + bg" being an
opaque cell that covers layers beneath it.

### Wide characters

Characters whose Unicode display width is 2 (East Asian wide, emoji,
some box-drawing in ambiguous-width mode) occupy **two consecutive
cell positions**. The character is anchored at the leftmost of the
two; the second position is occupied by the same character and is
not independently addressable or styleable. Composition, hit-testing,
and layout all treat the two positions as a single 2-cell unit.

Conforming clients MUST segment text into grapheme clusters before
measurement — a single visible character composed of multiple code
points (e.g. combining marks, emoji sequences) occupies the width
of the cluster, not the sum of its components.

Characters whose width is *ambiguous* under Unicode (some East Asian
punctuation, some box-drawing characters) are classified as narrow
(width 1) or wide (width 2) per client configuration. Clients MUST
expose this setting to users; content authors SHOULD avoid depending
on either classification for layout that must look identical across
clients.

## The tick

The external rendering cadence is **33ms per tick** (≈30 fps). On
each tick, a conforming client MUST process the following inputs
in this order before producing output:

1. **User input.** Key presses (and pointer events, if the client
   dispatches them) are applied first. These may change focus,
   scroll position, panel state, input mode, or navigate.
2. **Time advancement.** Each active animation produces its
   per-tick effect on the scene.
3. **Incoming protocol events.** Live-region updates and deltas
   from the server are applied.

The client then produces this tick's output:

4. **Layout.** Any content whose position or size was invalidated
   by the previous three steps is re-measured.
5. **Composition.** The display's layers are resolved into a
   single output cell grid per the Layering and composition
   section.
6. **Presentation.** The client emits ANSI for cells that differ
   from the previous tick's output.

**Ordering guarantees.**

- Within a tick, user input applies before animation output. If a
  user action and an animation frame affect the same property in
  the same tick, the user action wins; the animation MAY skip a
  frame.
- Changes of the same kind within a tick apply in arrival order.
- Across ticks, ordering of concurrent user input and server
  events is undefined — each tick boundary is a serialization
  point.

**Idle behavior.** A page with no animations, no live regions, and
no user input does zero work past stage 1. Stages 2–6 run only when
something has changed.

A client MAY decompose these six stages further (for example,
separating invalidation bookkeeping from layout), but the order of
effects on the rendered output MUST match the sequence above.

## Layering and composition

The display is a layered composition. Every piece of rendered content
belongs to exactly one layer. Layers have:

- **Position** — an offset within the output grid.
- **Z-index** — an integer. Higher z composites on top.
- **Transform** — a 2D offset from position (used for slide/wipe
  transitions). No scale, rotation, or perspective.
- **Clip rect** — the region within which a layer's cells are drawn.
  Cells outside the clip are discarded.

**Composition order.** Layers composite bottom-to-top by z-index.
Ties break by document order — earlier-declared layers at the same z
composite below later-declared layers at that z.

**Cell resolution.** For each cell position in the output:

1. Walk layers top-down in composition order.
2. The first layer with a *present* cell at that position wins.
3. If no layer has a present cell, the composited output cell is absent.
   During final ANSI presentation, the client materializes absent terminal
   colors as literal RGB white (`#ffffff`) on black (`#000000`) so the host
   terminal palette or theme cannot bleed through the canvas.

This rule applies uniformly: to a tooltip over live content, to a
dialog over a running background animation, to a panel transitioning
between states. It is the only composition rule.

## Transitions

A transition is an animation that swaps one content layer for another
over a duration. Seven transition kinds are defined; a conforming
client MUST implement all seven.

| Kind | Behavior |
|---|---|
| `Cut` | Instant swap. No animation. |
| `Fade` | Old layer dissolves to black over the first ~40% of the duration, the region holds black for ~20%, then the new layer dissolves in over the final ~40%. The per-cell flip to/from black is stochastic. |
| `SlideLeft` | Old layer translates off-region to the left while the new layer translates in from the right. Both layers retain their own cells; both are clipped to the region. |
| `SlideRight` | Old layer translates off-region to the right while the new layer translates in from the left. |
| `SlideUp` | Old layer translates off-region upward while the new layer translates in from below. |
| `SlideDown` | Old layer translates off-region downward while the new layer translates in from above. |
| `Dissolve` | Both layers present; a per-cell mask flips cells from old to new over the duration, in a client-chosen pattern (random, ordered, noise-based). Each cell is discretely old or new at any instant — no intermediate blend. |

**Shape-mismatched transitions.** When the outgoing and incoming
layers differ in size or position, every cell in the union region
resolves independently: cells present in both use the kind's rule;
cells exclusive to one source show that source until flipped; cells
in neither reveal the layer underneath. No special-casing — the cell
presence rule from the previous section handles it.

**Where transitions apply.**

- **Panel state transitions** are declared per-state in AML and fire
  when the panel's active state changes.
- **Page transitions** are declared per-link and fire on navigation.

**During a page transition**, input events are not dispatched to
either page — both are frozen for the transition's duration and
become interactive again when the transition completes. Panel-state
transitions do not freeze input; the surrounding page remains
interactive.

Cell presence, per-kind decision functions, position, z-index, and
clip rect are the complete vocabulary of transition behavior. No
alpha, no morphing, no per-cell color interpolation.

## Focus and hit-testing

**Focusable elements** (links, buttons, form inputs) form an ordered
sequence in document order. A conforming client MUST support
keyboard focus traversal:

- **Forward focus (`Tab`)** — move focus to the next focusable in
  document order, wrapping from last back to first.
- **Backward focus (`Shift-Tab`)** — move focus to the previous
  focusable in document order, wrapping from first back to last.

**Focus indication.** The focused element MUST be visually distinct
from unfocused focusables. The mechanism (reverse video, underline,
color shift, dedicated indicator glyph) is client-defined; conforming
clients MAY choose the convention that best suits their terminal and
user base. What is fixed: the user must be able to tell at a glance
which focusable will receive activation.

**Activation.** `Enter` (or client-equivalent activation key)
activates the focused element:

- A `[link]` navigates to its `href`.
- A `[button]` fires its declared action.
- An `[input]` enters input mode on the focused field.

**Scroll-into-view.** Focus traversal MUST NOT leave the viewport —
the client scrolls to keep the focused element visible on every
focus change.

**Hit-testing (for pointer-driven clients).** When a pointer event
occurs at screen coordinates `(x, y)`, a client that dispatches
pointer events MUST identify the deepest element whose screen
rectangle contains the point, considering z-order. Elements at
higher z-index take precedence; within the same z-index, later
document order wins. Whether a client dispatches pointer events at
all, and what actions a click produces (focus change, activation,
both), is client-defined — keyboard traversal is the only mandatory
input modality.

## Scrolling

Scrolling applies only in document mode (see Page modes and
viewport, below); screen-mode pages do not scroll. In document mode,
the client maintains a **scroll offset** (0 = top of document). A
conforming client MUST support:

- **Line-at-a-time scroll** — one row per key press. Bindings
  client-defined (arrow keys, `j`/`k`, wheel events, etc.).
- **Page-at-a-time scroll** — one viewport height per press.
  Bindings client-defined (typically `PageUp`/`PageDown`).
- **Jump-to-top / jump-to-bottom** — bindings client-defined
  (typically `Home`/`End` or `g`/`G`).
- **Scroll-into-view on focus change** — any focus transition that
  would place the focused element outside the viewport MUST scroll
  the viewport so the element is visible.

### Sticky regions

An element declared sticky (e.g. `[header sticky]`) MUST remain
pinned to its screen position while the rest of the document scrolls
beneath it. Sticky regions composite above scrolled content at
their declared z-index, per the layering rules in the Layering and
composition section.

## WASM animations

Animations declared with `[animation wasm src="..."]` run a sandboxed
WebAssembly module that produces cells within its declared region.
The **set of operations** a module can perform on the host is part
of this specification; the **binary calling convention** (function
signatures, data encoding) is an ABI concern documented in
`crates/dustnet-client/src/compositor/wasm.rs` in the reference implementation.

### Host operations

The host exposes exactly the following operations to the WASM
module. A conforming host MUST expose all of these, and MUST NOT
expose others — adding operations would make modules client-specific.

- **Set a cell.** Write a glyph with explicit foreground, background,
  and style into a given region-relative position.
- **Clear the region.** Mark every cell in the region as absent.
- **Query region dimensions.** Read the region's width and height in
  cells.
- **Obtain a pseudo-random value.** The single source of
  nondeterminism available to the module.
- **Read an underlying content cell.** Sample a cell from the
  region's content buffer, for effects that interact with existing
  text or art (e.g. a decode or scramble effect operating on
  pre-rendered content).

**Coordinate space.** All coordinates are **region-relative**. A
WASM module does not see, and cannot address, the rest of the page.
Its writes affect only its declared region. Writes outside the
region are silently discarded.

### Sandbox guarantees

- **No ambient authority.** The module receives no filesystem,
  network, wall-clock, environment, or IPC access. The six host
  operations above are the entire API surface.
- **Determinism boundary.** The pseudo-random operation is the only
  nondeterministic input. Conforming clients MAY seed it however
  they wish (fixed seed, per-page seed, OS entropy); authors MUST
  NOT depend on any particular seeding scheme.
- **Bounded resources.** Each module runs under two limits:
  - A **per-tick instruction budget** that scales linearly with the
    region's cell count, so a module that fits its budget on an
    80×24 region also fits on larger regions. Modules that exceed
    the budget are preempted; writes so far this tick are retained
    and execution resumes next tick.
  - A **bounded linear-memory cap** in the low megabyte range.
  The exact numeric constants are a versioned ABI detail; see the
  reference implementation. Conforming clients MAY choose different
  constants, but MUST preserve the area-linear scaling rule for
  the instruction budget.

### Lifecycle

An animation runs while its node is visible. Clients MAY pause
animations that are entirely outside the viewport (and not part of
an active transition) to save work, and MUST resume them when the
region becomes visible again.

What happens when an animation **declares itself finished** — unmount
the node, hold the final frame, or let the author declare the
behavior — is currently **unspecified**. Authors SHOULD NOT rely on
either behavior until the spec pins it down.

## Color

### Color model

Authors express colors in AML in one of three forms:

- **Named** — one of 16 standard terminal colors: `black`, `red`,
  `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`, and their
  `bright-` variants (`bright-black` through `bright-white`).
- **256-color palette** — `color(N)` where `N` is `0`–`255`, the
  standard xterm 256-color palette.
- **Truecolor** — `#rrggbb` hex, a 24-bit RGB value.

**Black is literal.** `black` names palette slot 0, which terminal
themes routinely set to something other than `#000000`. Because an
absent color is already materialized as literal `#000000` (see *Cell
resolution*), a client MUST present `black` as literal RGB `#000000`
rather than as the palette reference, at every color-support tier. The
two blacks have to agree: otherwise a region that says `bg=black`
bands visibly against the region beside it that says nothing at all.
`bright-black` is unaffected — it stays a palette reference, so themes
keep control of their greys.

### Client color support tiers

Every conforming client operates in exactly one of four color-
support tiers:

- **None** — no color output; all style (bold, italic, underline)
  preserved but colors dropped.
- **Basic** — the 16 named colors.
- **Palette256** — the 256-color palette.
- **Truecolor** — 24-bit RGB.

Clients MUST probe their terminal's capability and select the best
available tier automatically.

### Downsampling

When a requested color cannot be represented in the current tier,
the client MUST downsample along the following chain:

`Truecolor → Palette256 → Basic → None`

The downsampling algorithm (e.g. nearest palette entry by RGB
distance) is client-defined, but clients MUST produce *some* visible
color when the tier is Basic or richer and the source color is
non-None. Silently dropping colors above None is non-conforming.

## Style

A cell's style consists of a foreground color, a background color,
and zero or more of the following **six style flags**:

- `bold` — heightened weight or intensity.
- `italic` — italic or oblique rendering (terminal-dependent).
- `underline` — underlined glyph.
- `strikethrough` — glyph struck through.
- `dim` — reduced intensity.
- `blink` — blinking (terminals MAY render as non-blink if
  disabled by the user; authors SHOULD NOT rely on it as the sole
  indicator of state).

This list is exhaustive. Authors write flags as bare attributes:
`[text bold italic]...[/text]`. Conforming clients MUST emit the
appropriate ANSI SGR parameters for each supported flag; clients
MAY ignore individual flags their terminal cannot represent, but
MUST NOT substitute (e.g. rendering `italic` as `reverse`).

## Text rendering

### Grapheme segmentation

All measurement, wrapping, and cell placement operates on **grapheme
clusters** (per Unicode UAX #29), not raw code points. A character
composed of a base plus combining marks occupies one grapheme's
width, not the sum of its parts.

### Width

Each grapheme has a display width of 0, 1, or 2 cells:

- **Zero-width** — combining marks, zero-width joiners. Attach to
  the preceding grapheme; do not advance the cursor.
- **Narrow (1 cell)** — most Latin, Cyrillic, Greek characters.
- **Wide (2 cells)** — East Asian wide characters, most emoji, box-
  drawing in ambiguous-width-wide mode.

Clients MUST follow Unicode's width table for unambiguous cases.
For Unicode "ambiguous-width" characters, the width is
client-configurable — see the Cell Model section.

### Wrapping

In document mode, lines that exceed the viewport width wrap at
**grapheme-cluster boundaries**, preferring word boundaries where
practical. Authors MAY use `[pre]` for preformatted content that
must not wrap.

### Trailing whitespace

In document mode, trailing space cells at the end of a line MAY be
omitted from the output (the terminal fills them with the default
background). Authors who need guaranteed-painted trailing cells
(e.g. coloured backgrounds extending to the margin) MUST use an
explicit background via a container element.

## Page modes and viewport

AML documents declare a `mode` attribute on the root `[page]`
element: `document` or `screen`. The two modes differ in how they
relate content to the terminal's viewport.

### Document mode

Content flows vertically. Width fits the terminal. Height may
exceed the viewport.

- If content height > viewport height, the client MUST enable
  vertical scrolling (keyboard keys per Scrolling below).
- If content height ≤ viewport height, the document renders in the
  top portion of the viewport; cells below are absent (terminal
  default fill).
- On terminal resize, the client MUST re-wrap text at the new width
  and re-layout. Scroll offset MAY be preserved (by line index) or
  reset — client-defined.

### Screen mode

Content is authored at fixed `cols` × `rows`. No scrolling.

- If the terminal is **larger** than the declared canvas, the client
  MUST center the canvas in the viewport. Cells outside the canvas
  are absent (terminal default fill).
- If the terminal is **smaller** than the declared canvas, the
  client's behavior is one of: display an error / too-small message,
  clip the canvas and render what fits, or scale down (if the client
  implements scaling). Which of these is chosen is client-defined,
  but the client MUST make the situation visible to the user rather
  than silently clipping.
- On terminal resize, re-center; content does not re-layout.

### Viewport during transitions

During a page transition, both the outgoing and incoming pages are
rendered within the viewport according to their own mode rules; the
transition's per-cell decision function composes them.

## Non-goals

- **Not 60 fps.** 33 ms (≈30 fps) is the target tick. Sub-16 ms
  frame budgets are out of scope; terminal emulators rarely render
  reliably at those rates.
- **No alpha.** See the cell model. "Transparency" is cell absence.
- **No browser APIs.** No DOM, no JavaScript, no CSS. AML is the
  surface language.
- **No 3D.** Transforms are 2D offsets only.
- **No partial initial page streaming.** A page either loads
  atomically and replaces the display, or it does not. Incremental
  updates apply to an already-loaded page via the delta protocol.

## Relationship to other specs

- **AML syntax** → [03-markup.md](03-markup.md). What authors write.
- **ATP wire protocol** → [02-protocol.md](02-protocol.md). How
  pages and deltas arrive over the wire.
- **Interactivity semantics** →
  [06-interactivity.md](06-interactivity.md). Panels, forms,
  components, live content.
- **Security** → [07-security.md](07-security.md). Sandbox isolation
  and network safety guarantees.
- **Implementation architecture** →
  [compositor.md](../internals/compositor.md). How the reference client in
  `crates/dustnet-client/src/compositor/` realizes this specification.

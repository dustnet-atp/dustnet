# AML — Content Format

## Overview

AML (ANSI Markup Language) is Dustnet's declarative content format. It describes layout, text, color, interactive elements, and animation — but never logic or computation. AML is to ATP what HTML is to HTTP, with one critical difference: AML is not Turing-complete and contains no scripting capability.

## Design Goals

1. **Expressive enough** to create rich, beautiful terminal interfaces
2. **Simple enough** to parse and render safely in any language
3. **Constrained enough** that a malicious document cannot harm the client
4. **Adaptable** to different terminal sizes and color capabilities

## Syntax

AML uses an XML-like syntax with square brackets instead of angle brackets. Square brackets were chosen to avoid confusion with ANSI escape sequences and to feel more natural in a terminal context.

Tags have a name, optional attributes, and may contain children or be self-closing:

```
[tag attribute=value attribute2="value with spaces"]
  content
[/tag]

[selfclosing /]
```

Attributes may be:
- **Key-value**: `name=value` or `name="value with spaces"`
- **Flags**: bare attribute names like `bold`, `italic`, `underline` (no value, presence implies true)

## Escaping

Within AML content, literal bracket characters and backslashes are escaped:
- `[[` produces a literal `[`
- `]]` produces a literal `]`
- `\\` produces a literal `\`

Inside quoted attribute values, standard backslash escaping applies: `\"`, `\\`, `\n`, `\t`.

## Document Structure

Every AML document has a root `[page]` element with a mode attribute that determines layout behavior:

```
[page mode=document title="My Page"]
  [meta author="alice" description="A cool page" /]
  [style default-fg=white default-bg=black /]

  [header]...[/header]
  [body]...[/body]
  [footer]...[/footer]
[/page]
```

## Page Modes

### Document Mode

Content flows vertically. Text wraps at the terminal width. The page may be taller than the viewport — the client enables vertical scrolling with keyboard and mouse wheel input.

Use cases: feeds, articles, conversations, long-form content.

### Screen Mode

Content is positioned on a fixed canvas with explicit `cols` and `rows` dimensions. No scrolling. Elements may be placed at exact coordinates using `x` and `y` attributes.

If the terminal is larger than the canvas, the page is centered. If smaller, the client may scale or display a "terminal too small" message.

Use cases: splash pages, art displays, dashboards, games.

## Elements

### Layout Elements

**Box** (`[box]`) — A bordered rectangular container. Supports all six border styles (single, double, rounded, heavy, ascii, none), configurable width and height, padding, background color, content alignment, and an optional title displayed in the top border. Width defaults to fill (uses all available space) and height defaults to fit (wraps content). Optional `join-top`, `join-bottom`, `join-left`, and `join-right` offsets replace an edge cell with the matching box-drawing junction so animated connectors can meet the border without a visual gap.

**Row and Column** (`[row]`, `[col]`) — Horizontal layout. A row contains columns arranged side by side. Columns have configurable width (fixed or fill) with a gap between them. Rows support vertical alignment of their children (top, middle, bottom).

**Horizontal Rule** (`[hr]`) — A full-width divider line with configurable style (single, double, heavy, dash, dot, ascii) and color.

**Spacer** (`[spacer]`) — Vertical whitespace with a configurable number of blank lines.

**Structural containers** (`[header]`, `[body]`, `[footer]`, `[nav]`) — Semantic containers for organizing page sections. The `[nav]` element can be made sticky (pinned to the top or bottom of the viewport during scrolling).

### Text Elements

**Text** (`[text]`) — Styled inline text. Supports foreground and background color, bold, italic, underline, strikethrough, dim, blink, and alignment. Text elements may nest to create inline style changes — children of a `[text]` element (including nested `[text]`, `[link]`, and `[button]` elements) flow inline on the same line rather than stacking vertically. Word wrapping works across styled spans, preserving each span's style. This enables rich inline content like `[text]Click [link href="/x"][text bold]here[/text][/link] to continue[/text]` where "here" appears bold and underlined inline with the surrounding text.

Common inline patterns:

```
[text]                                                 Clickable name with inline metadata
  [link href="atp://radio.dust"]
    [text fg=bright-white bold]radio.dust[/text]
  [/link]
  [text dim] · added 2026-03-18[/text]
[/text]

[text]                                                 Link with inline description
  [link href="/sites"]
    [text fg=bright-white bold]Site Directory[/text]
  [/link]
  [text dim] — browse all known sites[/text]
[/text]

[text]                                                 Prose with embedded link
  Running an ATP server?
  [link href="/submit"][text fg=bright-white]Submit it[/text][/link]
  and it'll appear in the directory after review.
[/text]
```

**Preformatted** (`[pre]`) — Preserves whitespace and line breaks exactly as written, with no word wrapping. Essential for ASCII and ANSI art.

**Heading** (`[heading]`) — Section headings with level 1–3, rendered with appropriate size and weight emphasis.

**List** (`[list]`, `[item]`) — Bulleted or numbered lists. Supports bullet, number, dash, arrow, and none styles, with a configurable bullet character.

### Interactive Elements

**Link** (`[link]`) — Navigation to another page or site. Links are focusable; the client highlights the focused link and activates it on Enter. Links specify a target URI, an optional transition name, an optional keyboard shortcut, and may request prefetching. `defer="animation-id"` starts the named animation on activation and waits for its `animation-end` event before loading the destination, enabling authored exit sequences. When nested inside a `[text]` parent, links participate in inline flow — the link text appears inline with surrounding content rather than on its own line. Link text is automatically underlined.

**Input** (`[input]`) — A text input field with a name, maximum length, placeholder text, and optional multiline, password masking, and default value support.

**Select** (`[select]`, `[option]`) — A focusable selection control with named options. Enter advances to the next option and wraps at the end. A select requires at least one option and permits at most one initially selected option.

**Button** (`[button]`) — An activatable element. Buttons can submit forms, navigate to URIs, or trigger panel state changes.

**Form** (`[form]`) — Groups inputs and buttons for submission. When submitted, the client sends an INPUT message to the server with all field values.

### Media Elements

**Art** (`[art]`) — An ANSI/ASCII art block with explicit dimensions and character encoding. Supports UTF-8 (default), CP437 (classic IBM PC art), and PETSCII (Commodore). Art may be inline or loaded from an external file. An `alt` attribute provides a text description for accessibility.

**Table** (`[table]`, `[thead]`, `[tbody]`, `[tr]`, `[th]`, `[td]`) — Data tables with optional borders, header rows, and per-cell styling.

### Animation Elements

See [compositor.md](../internals/compositor.md) for the full rendering and animation
architecture.

**Frame animation** (`[animate]`, `[frame]`) — Pre-rendered frame sequences played at a declared FPS.

**Tween animation** (`[element]`, `[tween]`, `[at]`) — Smooth interpolation of position and color between keyframes.

**Text animation** (`[text-animate]`) — Text reveal effects like typewriter, scramble, and fade-in.

### Collapsible Elements

**Details** (`[details]`) — A collapsible container with a summary line. When collapsed, shows only the summary with a `▶` indicator. When expanded, shows `▼` plus all children indented. Supports arbitrary nesting for tree structures like comment threads. The `summary` attribute provides the header text; the `open` flag controls initial state (default: collapsed). Keyboard navigable: Tab to focus, Enter to toggle. See [06-interactivity.md](06-interactivity.md) for the full details specification.

### Event Binding Elements

**Event binding** (`[on]`) — A declarative, non-visual element that connects events to actions. When a specified event occurs (e.g. a page loads, an element gains focus, an animation finishes), the binding triggers an action on a target element (e.g. start an animation, set a panel state). See [06-interactivity.md](06-interactivity.md) for the full event binding specification.

### Live Content Elements

**Live region** (`[live]`) — A server-pushed content area. The client automatically subscribes when the page loads, and the server pushes AML fragment updates. Attributes:

| Attribute | Default | Description |
|-----------|---------|-------------|
| `id` | (required) | Region identifier, used to match UPDATE messages to regions |
| `endpoint` | (required) | Server path to subscribe to for updates |
| `height` | fit | Fixed height in rows. The layout reserves this space regardless of content size. |
| `scroll` | `none` | How incoming content is displayed: `none` (replace), `tail` (append, auto-scroll to bottom), `manual` (append, user scrolls), `prepend` (insert at top) |
| `buffer` | 100 | Maximum lines retained in the scroll buffer (max 1,000). Oldest content is discarded when exceeded. |
| `delta` | false | When present, the client requests incremental updates from the server instead of full replacements. Only suitable for append-only content sources. |

### Server-Resolved Content

**Include** (`[include name=... /]`) — A named placeholder that a server replaces with generated content before sending the page. Self-closing; an `[include]` has no children, because anything written inside one would be discarded on substitution.

| Attribute | Default | Description |
|-----------|---------|-------------|
| `name` | (required) | Which handler is expected to fill this placeholder |

A client never renders an `[include]`. One arriving over the wire means the origin has no handler for that name, and it contributes nothing — deliberately not the literal text, which would show the reader a marker rather than a page.

Two constraints keep this mechanism disjoint from components:

- `include` is a reserved element name, so a `[def]` cannot shadow it.
- An `[include]` may not appear inside a `[def]` body (error `E052`). A component body is copied into every call site with `$attr` substitution applied, and server content is substituted later, so an include expanded there would place generated content inside a region where `$` is still live.

Distinct from `[slot]`, which the component system owns for marking where a `[def]`'s caller content goes: that is resolved at parse time from within the document, this at serve time from outside it.

> **Status.** `dustnetd` does not resolve includes: it installs no resolver, serves authored AML unchanged, and a page it serves therefore renders its includes as nothing. A server built on `dustnet-server` that installs an include resolver replaces each placeholder before the page goes on the wire. See [06-interactivity.md](06-interactivity.md).

## Color System

Colors can be specified in three formats:

| Format | Example | Description |
|--------|---------|-------------|
| Named | `red`, `cyan`, `bright-white` | 16 standard terminal colors plus bright variants |
| 256-color | `color(196)` | xterm 256-color palette by index |
| Truecolor | `#ff6600` | 24-bit RGB hex |

The client maps colors to the terminal's negotiated color capability. Truecolor values are downsampled to 256 or 16 colors when the terminal doesn't support them, using perceptual distance matching. Colors inherit from parent to child — a box with `fg=cyan` will color all text inside it unless overridden.

## Text Encoding

AML documents are UTF-8. The `[art]` element supports additional legacy encodings for classic art formats:

- **UTF-8**: Full Unicode (default)
- **CP437**: IBM PC character set, common in classic ANSI art. The client maps CP437 bytes to Unicode equivalents.
- **PETSCII**: Commodore character set. The client maps to Unicode block element equivalents.

## Sticky Elements

In document mode, elements marked with `sticky=bottom` are pinned to the bottom of the viewport. The sticky content is extracted into a separate buffer after layout — it does not scroll with the document. Focus, input, and cursor positioning work correctly within sticky regions.

`sticky=bottom` is supported on `[nav]` elements. The sticky region must be at the end of the document — any content after it is included in the sticky area.

```
[nav sticky=bottom]
  [form action="/send"]
    [input name="msg" placeholder="Type here..." /]
    [button action=submit]Send[/button]
  [/form]
[/nav]
```

## Pagination

For long content, servers paginate rather than serving everything at once. A `[pagination]` element provides navigation links between pages. The `[meta next-page="..."]` attribute hints to the client that it may prefetch the next page for smoother navigation.

## Accessibility

- `[art alt="description"]` provides text descriptions of visual content
- The client maintains a focus order for interactive elements (links, inputs, buttons)
- Tab and Shift-Tab cycle focus; Enter activates the focused element
- `[heading]` elements provide document structure for navigation

## Document Limits

The client enforces hard limits on every document to prevent resource abuse:

| Limit | Value |
|-------|-------|
| Maximum document size | 1 MiB (uncompressed) |
| Maximum element nesting depth | 32 |
| Maximum total elements | 10,000 |
| Maximum attribute value length | 4,096 characters |
| Maximum text content per element | 64 KiB |
| Maximum art dimensions | 200 columns x 200 rows |
| Maximum explicit screen width | 512 columns |
| Maximum document coordinate | 4,096 rows |
| Maximum explicitly sized element | 2,048 cells per dimension |
| Maximum cells in one render buffer | 1,048,576 |
| Maximum live regions per connection | 16 |
| Maximum animate regions per page | 1,024 |
| Maximum WASM-backed animate regions per page | 16 (64 MiB aggregate linear memory) |
| Maximum animation frames total | 256 |
| Maximum event bindings per page | 32 |
| Maximum event cascade depth | 16 |
| Aggregate retained remote memory | 128 MiB |
| Cached resources | 32 entries / 8 MiB |
| Pending updates | 64 entries / 8 MiB |
| History | 128 entries / 16 MiB retained AML |

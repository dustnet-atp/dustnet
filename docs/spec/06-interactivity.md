# Interactivity

> Production-boundary note: panels, forms as client-side markup, and static
> file-backed live regions are supported. `dustnet-server` additionally provides
> optional hooks a server may install — include resolution, form submission and
> session resolution — and `dustnetd` installs none of them, so it authenticates
> nobody, generates nothing, and refuses `INPUT` with 405.
>
> Accounts, password storage, SMTP and dynamic poll handlers are *not* provided:
> those are a site's own, built on the hooks. The prototype under
> `examples/unsupported-social` remains unsupported and is not the way to build
> them — its plugin dispatch and `{{marker}}` substitution are superseded by
> `[include]` and `dustnet_core::serialize`.

## Design Constraint

Dustnet sites achieve interactivity through **state machines, not scripts**. Every possible visual state is declared in the document. User actions select between them. The client animates the transition. There is no conditional logic, no data flow, and no computation.

## Panels and States

A panel is a region of the page that can exist in one of several declared state configurations. Each state defines the panel's content, dimensions, and visual properties. The panel starts in its declared initial state.

When a trigger fires, the client:
1. Looks up the target panel
2. Determines the new state
3. Re-lays out the panel region with the new state's content
4. Animates the transition (if the target state declares one)

In document mode, changing a panel's height causes surrounding content to reflow — the transition animation covers the reflow so it looks smooth. In screen mode, panels occupy their absolute position and expanding panels render on top of neighbors.

### State Properties

Each state within a panel can specify:
- Its own content (any valid AML elements)
- A transition effect for animating into this state (`cut`, `fade`, `slide-*`,
  `draw-down`, `draw-right`, `draw-out`, or `dissolve`). Draw transitions construct one
  edge first, then reveal the panel by complete rows or columns.
- A transition duration
- Dimension overrides (width, height) that differ from other states

## Triggers

### Button Triggers

Buttons are the primary trigger mechanism:

- **Toggle**: Cycles through a declared list of states. Each activation advances to the next state, wrapping around to the first.
- **Set**: Jumps directly to a named state.

### Focus and Blur Triggers

Interactive elements (inputs, links, buttons) can trigger state changes when they gain or lose keyboard focus. This enables patterns like expanding a search panel when its input field is focused.

### Hover Triggers

On terminals that support mouse tracking, elements can trigger state changes on mouse hover and unhover. These degrade gracefully — on terminals without mouse support, the panel stays in its initial state.

### Trigger Safety

Trigger attributes (`trigger-focus`, `trigger-blur`, etc.) follow the one-action-per-trigger rule: a state change on one panel cannot directly trigger a state change on another panel through trigger attributes. For more complex choreography that connects multiple elements, use event bindings (below).

States contain static content. There is no mechanism to pass data between panels, read a panel's current state in AML, conditionally render content, or bind input values to panel content. State changes are purely client-side — the server is not notified.

## Event Bindings

Event bindings (`[on]`) are declarative rules that connect events to actions across elements. They are the choreography layer — the mechanism for creating cinematic, interactive experiences where user actions and animation completions trigger cascading visual responses.

Navigation links may also defer loading until an exit animation finishes:

```
[animate id="exit" src="/effects/atomise.wasm" autoplay=false]...[/animate]
[link href="/next" defer="exit"][text]Continue[/text][/link]
```

The client captures the selected destination, starts `exit`, and navigates only after that animation ends. Repeated activation while the exit is running is ignored.

### Syntax

```
[on event="EVENT" source="ELEMENT_ID" do="ACTION" target="TARGET_ID" to="STATE" delay="DURATION" /]
```

Event bindings are always self-closing. They produce no visual output and do not affect layout — they are metadata collected during layout and dispatched at runtime.

### Events

| Event | Fires when... | Source required? |
|-------|---------------|------------------|
| `page-load` | Page finishes initial render | No |
| `focus` | User tabs/arrows to a focusable element | Optional (matches by element ID) |
| `blur` | User leaves a focusable element | Optional (matches by element ID) |
| `animation-end` | A named animation completes | Yes (animation ID) |
| `state-change` | A panel transitions to a new state | Optional (panel ID) |
| `scroll-into-view` | Element enters the viewport | Optional |
| `select` | User activates (Enter) a focusable element | Optional |

When `source` is omitted, the binding matches any occurrence of that event type. When specified, only events from the named element trigger the binding.

### Actions

| Action | Effect | Requires `to`? |
|--------|--------|----------------|
| `animate` | Start or restart a named animation | No |
| `stop` | Stop a running animation | No |
| `set` | Set a panel to a specific state | Yes |
| `toggle` | Cycle a panel through its declared states | No |

### Delay

The optional `delay` attribute accepts a duration value (e.g. `500ms`, `1.5s`, `200`) specifying how long to wait after the event fires before executing the action. This enables staggered choreography — multiple bindings on the same event with different delays create timed sequences.

### Examples

**Page-load cinematic**: Start animations in sequence when the page loads.

```
[animate id="hero" fps=10 autoplay=false]
  [frame][pre]█ █ █ ██ ██[/pre][/frame]
  [frame][pre]H E L L O![/pre][/frame]
[/animate]

[animate id="subtitle" fps=10 autoplay=false]
  [frame][text dim]...[/text][/frame]
  [frame][text]Welcome to DUSTNET[/text][/frame]
[/animate]

[on event="page-load" do="animate" target="hero" /]
[on event="page-load" do="animate" target="subtitle" delay="800ms" /]
```

**Tab-select preview**: Focus on different entries updates a preview panel.

```
[panel id="preview" state="empty"]
  [state name="empty"][text dim]Select an entry...[/text][/state]
  [state name="entry-1" transition="fade" duration="200ms"]
    [text]First entry preview[/text]
  [/state]
  [state name="entry-2" transition="fade" duration="200ms"]
    [text]Second entry preview[/text]
  [/state]
[/panel]

[link id="e1" href="/entry/1"][text]Entry One[/text][/link]
[link id="e2" href="/entry/2"][text]Entry Two[/text][/link]

[on event="focus" source="e1" do="set" target="preview" to="entry-1" /]
[on event="focus" source="e2" do="set" target="preview" to="entry-2" /]
```

**Animation chaining with events**: Play a sequence where each animation triggers the next.

```
[animate id="step1" fps=10 autoplay=false]...[/animate]
[animate id="step2" fps=10 autoplay=false]...[/animate]
[animate id="step3" fps=10 autoplay=false]...[/animate]

[on event="page-load" do="animate" target="step1" /]
[on event="animation-end" source="step1" do="animate" target="step2" delay="200ms" /]
[on event="animation-end" source="step2" do="animate" target="step3" delay="200ms" /]
```

**Animated connector**: A site can provide the bundled `line-draw.wasm`
effect and author an orthogonal path with box-drawing characters. Non-empty
cells are revealed top-to-bottom and left-to-right; completion can trigger a
`draw-right` panel reveal.

```
[animate id="connector" x=20 y=10 src="/effects/line-draw.wasm" fps=8 autoplay=false]
  [pre]│
└────[/pre]
[/animate]

[panel id="destination" state="hidden"]
  [state name="hidden"]
    [box x=25 y=11 w=20 h=5 border=none][/box]
  [/state]
  [state name="visible" transition="draw-right" duration="800ms"]
    [box x=25 y=11 w=20 h=5 border=rounded]Destination[/box]
  [/state]
[/panel]

[on event="animation-end" source="connector" do="set" target="destination" to="visible" /]
```

**State-change cascade**: When a panel changes state, trigger an animation.

```
[animate id="glow" fps=10 autoplay=false loop=true]...[/animate]

[panel id="mode" state="off"]
  [state name="off"][text dim]OFF[/text][/state]
  [state name="on"][text bold fg=green]ON[/text][/state]
[/panel]

[button action="toggle" target="mode" states="off,on"]Toggle[/button]

[on event="state-change" source="mode" do="animate" target="glow" /]
```

### Server-Side Generation

Event bindings are plain AML, so any ATP implementation can generate them
alongside other content without adding executable logic to the client.
`dustnetd` serves authored AML verbatim, having no resolver installed.

A server that does generate them should compose tokens and let
`dustnet_core::serialize::to_aml` write the characters, for the reason
`docs/spec/07-security.md` gives under AML Injection. The example below does the
opposite — it comes from the unsupported prototype, and is shown as the shape to
recognise rather than the shape to copy:

```rust
// In a plugin's render method
for (i, entry) in entries.iter().enumerate() {
    aml += &format!(
        r#"[on event="focus" source="e{i}" do="set" target="preview" to="s{i}" /]"#
    );
}
```

### Cascade Behavior

Actions can trigger further events. For example, a `set` action causes a `state-change` event, which may match another binding. This enables multi-step choreography.

Cascade depth is capped at 4 to prevent infinite loops. The cascade terminates naturally in most cases — `set` to a state the panel is already in returns false and does not fire a state-change event.

### Event Binding Limits

| Limit | Value |
|-------|-------|
| Maximum bindings per page | 32 |
| Maximum cascade depth | 4 |

### Validation

All event bindings are validated at parse time:

- `source` must reference an existing element ID (animate, panel, live, or element-def)
- `target` must reference an existing element ID
- For `set` actions, the `to` state must exist on the target panel
- `animation-end` events require a `source` attribute
- Cascade depth is analyzed and warned if it exceeds the maximum

## Common Patterns

Panels, triggers, and event bindings enable a variety of UI patterns without scripting:

- **Accordion**: Panel with collapsed (1-line summary) and expanded (full content) states, toggled by a button
- **Tabs**: Panel with one state per tab, switched by set-buttons in a row
- **Toggle switch**: Panel with on/off states showing different text/colors
- **Dropdown menu**: Panel with closed (1-line) and open (menu list) states
- **Sliding reveal**: Panel with hidden and visible states, using slide transitions
- **Tooltip**: Panel triggered by hover, appearing on mouse-over
- **Cinematic intro**: Sequenced animations triggered by page-load and chained via animation-end events
- **Interactive preview**: Panel state changes driven by focus events on a list of links
- **Reactive indicators**: Animations started or stopped in response to panel state changes

## Forms and Input

### Input Handling

Forms are first-class ownership boundaries. Each input, select, and submit button belongs to its nearest enclosing form. Activating a submit button collects only that form's controls, in document order, and sends them to the declared action. Values never carry across forms or page navigation, and repeated field names remain repeated on the wire. If the client holds a session token whose scope matches the target path, it is included automatically.

Interactive elements include:
- **Text input**: Single-line or multiline text entry with maximum length, placeholder text, and optional password masking
- **Select menu**: Choice from a list of named options. Enter advances to the next option, wrapping at the end.
- **Button**: Activates submission or navigation

### Focus Navigation

The client maintains a focus order across all interactive elements on the page, including links and buttons nested inside `[text]` elements (inline layout). Tab and Shift-Tab cycle focus forward and backward. Enter activates the focused element (follows a link, presses a button, or submits a form). Keyboard shortcuts declared on links and buttons (the `key` attribute) provide direct activation.

### Address Navigation

Users can navigate to arbitrary URIs via the command line. Pressing `:` opens a vim-style command prompt; pressing `o` opens the prompt with `:open ` pre-filled. The command `:open <uri>` (or `:o <uri>`) fetches the target page over ATP. The URI can be absolute (`atp://host/path`), relative (resolved against the current page), or a bare hostname. If the target is on a different host, the client closes the existing connection and opens a new one. Cross-site navigation shares the same history stack, so back/forward navigation works seamlessly across sites.

The command line maintains a history of previously entered commands, navigable with Up/Down arrow keys.

## Protocol Authentication (Not Provided by `dustnetd`)

ATP defines session directives and the reference client implements their
path-scoped storage and attachment rules.

`dustnetd` does not authenticate users, issue or validate sessions, or implement
account routes; it rejects `INPUT` with status 405, and
`without_a_handler_input_is_still_refused` holds it to that.

The `dustnet-server` library it is built from does provide the pieces a server
needs to do those things: an input handler for form submissions and a session
resolver that turns a presented token into an identity. Neither is installed
unless a server asks for it, and what a server does with them — accounts,
password storage, rate limiting, email — is entirely that server's, because none
of it is something a generic ATP server can decide. `docs/spec/07-security.md`
states the boundary the library does enforce: a handler is given the identity and
never the token.

The workflows below describe how such a server uses the protocol. They are not
`dustnetd` features, and they are not the quarantined
`examples/unsupported-social` prototype either — that remains unsupported, and
its string-concatenation approach to generating AML is the thing
`dustnet_core::serialize` exists to replace.

Authentication uses the standard form submission mechanism — there is no special login element or handshake. A site serves a login page with input fields, the user submits credentials via a normal form, and the server responds with a session token if credentials are valid.

### Anonymous by Default

Sites work without authentication. No page should demand login unless it genuinely needs user identity. The site root and all read-only pages are accessible without credentials. Features that require identity (posting, commenting, voting) show "log in" links to unauthenticated users and forms to authenticated users.

### Server-Side Conditional Rendering

AML has no conditionals — it is purely declarative. Authentication-aware UI is achieved entirely at the server level. When the server receives a GET request, it resolves the session token to a verified username and generates different AML accordingly:

- **Authenticated**: The server includes forms (submit, comment, reply), action links, and user-specific content in the response.
- **Not authenticated**: The server omits those forms and instead includes a link to the login page (e.g., "Log in to comment").

This is the same model as server-side rendering on the web. The client remains a pure renderer with no conditional logic. The server decides what to send based on whether the session token resolves to a valid identity.

In the unsupported prototype, server-side plugins receive the **verified
username** (not the raw session token) as a parameter. That example resolves
the token before dispatching to plugins, including authenticated poll
subscriptions.

### Registration Flow

Registration requires the server operator to configure SMTP (via `SMTP_HOST`, `SMTP_USER`, `SMTP_PASS`, `SMTP_FROM` environment variables). If SMTP is not configured, registration is unavailable and the server rejects registration attempts.

1. The user navigates to `/register` — a page with fields for handle, email, and password
2. The server validates uniqueness and email format, hashes the password with argon2, generates an 8-character verification code, and sends it to the user's email
3. The user navigates to `/verify` and enters the code
4. On success, the account is activated and the user can log in

### Login Flow

1. The user navigates to a login page (e.g., `/login`) — a normal AML page with input fields for a handle and password
2. The user submits credentials via a standard form submission (INPUT message)
3. The server verifies the password against the argon2 hash in the user store, generates a CSPRNG session token (32 random bytes, hex-encoded), stores it in the server-side session map, and responds with a PAGE containing a `Set-Session` directive
4. The client stores the token, scoped to the declared absolute path
5. Subsequent requests whose path matches the scope automatically include the token
6. The server resolves the token to a username and generates auth-aware AML, showing forms and actions the user is authorized to use

### Logout Flow

1. The user navigates to `/logout` and submits the form
2. The server removes the session from its server-side session store and responds with a `Clear-Session` directive
3. The client discards the stored token

### Path-Scoped Sessions

A site may have multiple authenticated sections with independent credentials. For example, `/admin/` might require administrator credentials while `/members/` requires a separate member login. The server issues session tokens scoped to absolute paths, and the client only sends a token when the request path matches that scope on a complete path-segment boundary.

This means a moderator's admin token is never sent on requests to the member area, and vice versa. The client automatically selects the most specific matching token for each request.

### Session Management

The client provides a way to view all active sessions (site, scope, expiration), forget individual sessions, or clear all sessions for a site. The server can invalidate sessions by sending a `Clear-Session` directive in a response (used for logout flows).

See [02-protocol.md](02-protocol.md) for the full protocol-level specification of session tokens, scoping, and wire format.

## Live Content

### Live Regions

Live regions receive real-time content updates pushed by the server. When a page containing live regions loads, the client automatically sends SUBSCRIBE messages for each region (with `Mode: delta` if the region has the `delta` attribute). The server then pushes UPDATE messages containing AML fragments. Before navigating away or re-subscribing, the client sends UNSUBSCRIBE to cancel active subscriptions.

### Scroll Behavior

Live regions support four scroll modes for handling incoming content:

| Mode | Behavior |
|------|----------|
| `none` | New content replaces the entire region. Default. |
| `tail` | New content appends at bottom, auto-scrolls to show latest. Used for chat, logs, feeds. |
| `manual` | New content appends at bottom, user controls scroll position. |
| `prepend` | New content inserts at the top. Used for reverse-chronological feeds. |

The client maintains an in-memory `RegionBuffer` per live region for modes that accumulate content (`tail`, `manual`, `prepend`). For `none`, each update fully replaces the previous content.

### Delta Updates

Live regions can opt into delta (incremental) updates with the `delta` attribute:

```
[live id="chat" endpoint="/chat-stream" scroll=tail buffer=500 delta]
  [text dim]Loading...[/text]
[/live]
```

When `delta` is present, the client sends `Mode: delta` in its SUBSCRIBE message. The server tracks how many bytes it has sent per subscriber. When the watched file grows and the existing prefix is unchanged (verified by hash), the server sends only the new tail with the DELTA flag. If the file is rewritten or truncated, the server falls back to a full replacement.

Delta mode is appropriate for append-only content sources (e.g., chat logs). Content sources that rewrite existing data should not use delta mode — the server will detect the prefix change and fall back, but with unnecessary overhead.

### Buffer Management

Each live region has a configurable buffer limit (default 100 lines, maximum 1,000). When the buffer fills, the oldest content is discarded (from the top for `tail`/`manual`, from the bottom for `prepend`). This prevents unbounded memory growth from long-running connections.

### Poll-Based Subscriptions

Production `StaticServer` live regions are file-backed: it periodically checks
the bounded endpoint file and pushes an update when its content changes.
Server-generated dynamic content such as clocks or connection gauges requires
a different server implementation.

The unsupported social example implements that alternative with a plugin
`polls()` hook and virtual `/__poll/{name}` endpoints. This hook and endpoint
namespace do not exist in `dustnet-server` or `dustnetd`.

Poll subscriptions are cleared when the client navigates away (GET) or explicitly unsubscribes.

### Coexistence with Animation

Live regions and animations operate independently on the same page. Live regions update when the server pushes data. Animations update on the client's frame tick. Both participate in the dirty-region rendering system — only changed cells are redrawn. A live region's pushed content may itself contain animated elements.

## Components

### Purpose

Components are reusable AML templates that reduce repetition. They are macros — expanded at parse time into concrete AML. The layout engine, animation system, and renderer never see component references.

Components have no state, no lifecycle, and no runtime identity. They are purely syntactic sugar.

### Definition and Usage

A component is defined with `[def]`, specifying a name, configurable attributes, and named content slots. Once defined, the component name becomes a valid tag. When used, the parser substitutes attribute values and maps caller children to slots, producing expanded AML.

Attributes support default values. Slots support default content that is used when the caller doesn't provide anything for that slot.

### Expansion Rules

Component expansion happens in a single pass during parsing:
1. First pass: collect all component definitions into a registry
2. Second pass: parse the document, expanding component usages as they are encountered
3. The resulting AST is fully expanded with no component references

### Safety Constraints

The most important safety rule: **components cannot reference other components** in their definitions, and cannot reference themselves. This guarantees:
- Expansion always terminates
- Expansion depth is always exactly 1
- The expanded output size is bounded
- No component can amplify itself

If a pattern requires deeper composition, the server should expand it before sending.

### Component Limits

| Limit | Value |
|-------|-------|
| Maximum component definitions per document | 32 |
| Maximum usages per component | 64 |
| Maximum total usages (all components) | 256 |
| Maximum attributes per component | 16 |
| Maximum slots per component | 8 |
| Maximum expansion nesting depth | 1 |

Expanded elements count toward the global 10,000 element limit and 1 MiB document size limit.

## Details (Collapsible Containers)

The `[details]` element provides a lightweight collapsible container, independent of the panel/state system. It is designed for deeply nested, repetitive structures like comment trees where creating a panel per node would be impractical.

### Syntax

```
[details summary="Section Title" open]
  Content shown when expanded...
[/details]
```

### Attributes

| Attribute | Type | Default | Description |
|-----------|------|---------|-------------|
| `summary` | string | (required) | Text shown on the summary/header line |
| `open` | flag | (closed) | Whether the element starts expanded |

### Behavior

- **Collapsed**: Shows `▶ summary text` on a single line
- **Expanded**: Shows `▼ summary text` followed by indented children
- **Toggle**: Tab to focus the summary line, Enter to toggle open/closed
- **Nesting**: Details elements nest arbitrarily — collapsing a parent hides all descendants
- **Client-side**: The client tracks open/closed state per details element. No server round-trip.
- **Re-layout**: Toggling triggers a re-layout since content height changes

### Use Cases

- Comment trees (arbitrarily deep threading)
- FAQ sections
- Collapsible log output
- Nested navigation menus

## Panel, Details, and Event Binding Limits

| Limit | Value |
|-------|-------|
| Maximum panels per page | 64 |
| Maximum states per panel | 16 |
| Maximum transition duration (panel) | 1 second |
| Maximum trigger attributes per element | 4 |
| Maximum event bindings per page | 32 |
| Maximum event cascade depth | 4 |
| Maximum details elements per page | 256 |

## Validation

All trigger references, event bindings, and component usages are validated at parse time. The parser reports specific error codes for missing panels, missing states, missing event sources/targets, duplicate names, usage limit violations, cascade depth violations, and other structural issues. This means a well-formed document is guaranteed to have all its interactive elements correctly wired before rendering begins.

## Build-Time Markers

FIGlet banners are a build-time concern, not a server or client one, and never
generated while serving or rendering a page. Site source may use the bounded
`{{figlet:...}}` marker consumed by the standalone `tools/prerender-figlet`
package. The site build resolves only a pinned site-local font, emits ordinary
static `[pre]...[/pre]` AML into `target/sites`, and omits fonts, hidden state
and raw markers from the served tree. The `site` Make target validates every
emitted AML page; the server targets serve only that generated directory.

An unsupported prototype server also used runtime `{{name}}` text markers for
dynamic content. That mechanism is not part of the production boundary and is
described in
[`examples/unsupported-social/README.md`](../../examples/unsupported-social/README.md).

The supported equivalent is the `[include name=... /]` element in
[03-markup.md](03-markup.md): a placeholder the parser validates, which a server
with an include resolver installed replaces before the page goes on the wire. A
marker nothing expands is a diagnostic; a text marker nothing expanded was served
to readers as literal text, which is the difference that motivated the change.

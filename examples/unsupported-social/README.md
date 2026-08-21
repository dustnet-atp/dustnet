# `unsupported-social` — historical example

> **This is not part of Dustnet's production boundary.**
>
> This code is excluded from the Cargo workspace. It is not built, tested, or
> audited by CI, receives no security support, and is loaded by no production
> binary. `dustnet-server` and `dustnetd` provide a static, plugin-free
> `StaticServer` with no authentication, no custom-handler API, and no plugin
> surface — CI asserts that the production server cannot import this crate.
>
> It is retained as a record of an earlier dynamic-server prototype. Treat
> everything below as a description of unsupported prototype code, not as a
> statement about what Dustnet ships.

The supported security model is [`docs/spec/07-security.md`](../../docs/spec/07-security.md).

## What this prototype contained

A dynamic ATP server with accounts, email verification, boards, chat, links,
stats, and a plugin dispatch layer.

### Password hashing

Passwords are hashed with argon2 (via the `argon2` crate with default
parameters). Plaintext passwords are never stored. Salts are generated via
`getrandom` (16 bytes, base64-encoded).

### Session tokens

Tokens are 32 bytes from a CSPRNG (`getrandom`), hex-encoded to 64 characters.
Tokens are opaque, unguessable, and leak no information about the user. Tokens
expire after 24 hours.

### Server-side session store

The example maintains a bounded `HashMap<String, SessionInfo>` mapping tokens to
usernames. This is not a `StaticServer` facility.

### Identity resolution

Before dispatching requests or poll subscriptions to plugins, the server
resolves the raw session token to a verified username. Plugins receive
`identity: Option<&str>` (the username), never the raw token, so a plugin
cannot forge identity or misuse tokens — the trust boundary sits at the
`FileHandler` level.

### Persistent auth state

Users, pending registrations, and sessions are stored together in
`site_root/.auth/state.tsv`. Every logical mutation rewrites that complete
state through a sibling temporary file, `fsync`, and atomic rename, so account
verification cannot commit one entity while failing to commit another. The
loader performs a one-way compatibility migration from the earlier
`users.tsv`, `pending.tsv`, and `sessions.tsv` layout.

### Email verification

Registration requires email verification. The server generates an 8-character
alphanumeric code (36^8 ≈ 2.8 trillion combinations) and sends it via SMTP.
Codes expire after 1 hour. SMTP must be configured by the operator via
`SMTP_HOST`, `SMTP_USER`, `SMTP_PASS`, and `SMTP_FROM`; registration is
unavailable if SMTP is not configured.

### Rate limiting

Auth endpoints have separate rate limits from the per-plugin post rate limiter:

| Route | Limit |
|-------|-------|
| Login (`/login`) | 5 attempts per IP per 15 minutes |
| Registration (`/register`) | 3 attempts per IP per hour |

### Auth routes

Login, logout, registration, and verification are hardcoded routes (`/login`,
`/logout`, `/register`, `/verify`) handled directly by `FileHandler`, not by
the plugin system, so auth logic is not pluggable or overridable.

### Session invalidation

The `/logout` handler destroys the server-side session and sends
`Clear-Session` to the client. Even if the client ignores the directive, the
token is no longer valid in the example's store.

## Plugin and template-marker mechanism

This is how the prototype generated dynamic content. It moved here from the
interactivity specification, which describes only the supported boundary. The
production `StaticServer` performs bounded static AML, resource, WASM and
live-file serving only; it rejects form submissions and has no plugin,
authentication, template-marker or custom-handler interface.

### Template Markers

The unsupported prototype's server-side plugins provide dynamic content by replacing template markers in AML pages. A marker is a `{{name}}` placeholder embedded in the page source. When that example server serves the page, each registered plugin checks for its marker and replaces it with generated AML content.

The quarantined implementation supports two marker shapes:

- **Fixed markers** like `{{messages}}` or `{{links}}` — the plugin claims the exact string and replaces it with rendered content.
- **Parameterized markers** — a plugin claims a prefix and receives the text
  before the closing `}}`. No built-in unsupported plugin currently uses this
  form.

### Authentication in Plugins

In the unsupported prototype, plugins receive the **verified username** (not the raw session token) as `identity: Option<&str>` on every render and form submission. Its `FileHandler` resolves the session token to a username via the example's server-side session store before dispatching to plugins.

This allows plugins to adapt their output based on authentication state — showing forms to logged-in users and "log in" links to anonymous users, or rejecting form submissions from unauthenticated clients. Plugins that require authentication (e.g., the link aggregator) use the verified identity for attribution instead of a client-supplied name field.

The server handles login, logout, registration, and verification form submissions on hardcoded routes (`/login`, `/logout`, `/register`, `/verify`). Plugins do not manage sessions or authentication directly.

### Source Layout

The historical files live under
`examples/unsupported-social/src/legacy/`:

```
examples/unsupported-social/src/legacy/
  mod.rs                  # Server infrastructure, PagePlugin trait, FileHandler
  auth.rs                 # User store, session store, password hashing, rate limiting
  email.rs                # SMTP email delivery for registration verification
  plugins/
    mod.rs                # Re-exports all plugins
    board.rs              # Board plugin ({{messages}})
    links.rs              # Link aggregator plugin ({{links}})
    stats.rs              # Stats plugin ({{stats}})
```

### Adding a Plugin

The following historical extension recipe applies only inside the unsupported
example. It does not extend `StaticServer`:

```rust
// examples/unsupported-social/src/legacy/plugins/example.rs

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use super::super::{FormData, PagePlugin};

pub(crate) struct ExamplePlugin;

impl ExamplePlugin {
    pub(crate) fn new() -> Self { ExamplePlugin }
}

impl PagePlugin for ExamplePlugin {
    fn marker(&self) -> &str {
        "{{example}}"
    }

    fn render(
        &mut self, aml_path: &Path, query: Option<&str>,
        peer: SocketAddr, param: Option<&str>,
        site_root: &Path, identity: Option<&str>,
    ) -> String {
        // identity is Some("username") if authenticated, None otherwise
        "  [text fg=cyan]Hello from the example plugin![/text]\n".to_string()
    }

    fn handle_input(
        &mut self, aml_path: &Path, fields: &FormData,
        query: Option<&str>, identity: Option<&str>,
    ) -> Result<bool, String> {
        Ok(false) // No form handling
    }
}
```

Then register it in two places:

1. **The example's `src/legacy/plugins/mod.rs`** — add the module and re-export:
   ```rust
   mod example;
   pub(crate) use example::ExamplePlugin;
   ```

2. **The example's server module** — add to its plugin list in `FileHandler::new()`:
   ```rust
   plugins: Mutex::new(vec![
       Box::new(BoardPlugin::new()),
       Box::new(HnPlugin::new()),
       Box::new(ExamplePlugin::new()),
   ]),
   ```

The `PagePlugin` trait provides these methods:

| Method | Purpose |
|--------|---------|
| `marker()` | Returns the template marker string (e.g. `"{{example}}"`) |
| `is_parameterized()` | Override to return `true` for prefix-style markers (default: `false`) |
| `polls()` | Override to return `true` for content re-rendered once per second (default: `false`). Content is auto-wrapped in a `[live]` region |
| `render()` | Generate AML content replacing the marker on GET requests |
| `handle_input()` | Handle form submissions — return `Ok(true)` if data was stored, `Err(msg)` to show an error |

Shared helper functions available to plugins via `super::super::`:

| Function | Purpose |
|----------|---------|
| `sanitize_user_content(s)` | Escape `[]$` in user input for safe AML embedding |
| `sanitize_field(s, max)` | Strip terminal escapes, trim, and truncate a form field |
| `format_time_ago(timestamp)` | Format a Unix timestamp as "3 hours ago" etc. |
| `extract_domain(url)` | Extract domain from a URL for display |
| `parse_form_data(s)` | Parse ordered URL-encoded form data without collapsing duplicate names |

### Built-in Plugins

**Board** (`{{messages}}`) — A message board. Renders posted messages and handles form submissions for new messages. Currently allows anonymous posting. Data is stored in `{page}.board.tsv` (tab-separated: timestamp, name, body).

**Link Aggregator** (`{{links}}`) — An HN-style link aggregator with voting, threaded comments, and link submission. Requires authentication: the submit form and comment/reply links are only shown to logged-in users. Posts and comments are attributed to the verified username from the session — there is no client-supplied name field. Unauthenticated users see the link list and can read comments, but see "log in" links where forms would appear. Form submissions from unauthenticated clients are rejected with an error. Data is stored in `{page}.links.tsv` (tab-separated: id, timestamp, user, title, url, votes) and `{page}.links.comments.tsv` (tab-separated: id, link_id, parent_id, timestamp, user, text). Links are ranked using HN-style gravity: `score / (age_hours + 2)^1.5`. Comments are threaded via `parent_id` (0 = top-level) with a maximum nesting depth of 10.

**Stats** (`{{stats}}`) — Live server statistics: connected client count, server uptime, and UTC clock. This is a poll-based plugin, so its output is automatically wrapped in a `[live]` region and pushed to the client every second. Uptime is displayed as `Xd Xh Xm`, `Xh Xm`, `Xm Xs`, or `Xs` depending on magnitude.

A page may contain multiple dynamic plugin markers in this unsupported example.

Render-only markers do not participate in form routing. A page with one input-capable plugin routes submissions automatically. A page with more than one must select a handler in each form action using the reserved `__handler` query parameter, for example `[form action="/community?__handler=board"]`. Ambiguous and unknown handlers are rejected rather than resolved by plugin registration order.

## Checklist for a dynamic ATP server

If you are writing your own session-aware ATP server — outside Dustnet's
supported production boundary — the properties this prototype aimed at are a
reasonable starting point. They are requirements on *your* implementation;
Dustnet does not verify any of them.

- Passwords hashed with argon2 (never stored in plaintext)
- Session tokens are CSPRNG-generated (32+ bytes of randomness)
- Server-side session store validates tokens on every request
- Session expiry enforced server-side (not just client-side)
- Logout destroys the server-side session (not just the client token)
- Login, registration, and verification endpoints are rate-limited per IP
- Email verification codes are sufficiently long (8+ alphanumeric characters)
- Verification codes expire within a bounded window (1 hour)
- User input re-validated on the server (never trust client-side limits alone)
- Auth file writes are atomic (write-to-tmp then rename)
- Plugins receive resolved identity, not raw session tokens
- Auth routes are hardcoded, not plugin-overridable
- User content is escaped before embedding in AML, including AML's bracket and
  component-substitution metacharacters

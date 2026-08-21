# ATP — Wire Protocol

## Transport

ATP runs over **TLS 1.3 on TCP**. The default port is **1985** — the year FidoNet echomail and the WELL launched, early pioneers of networked online community.

TLS over TCP provides encryption and integrity by default, server authentication via certificates (compatible with Let's Encrypt), and familiar infrastructure without the shell semantics of SSH or the insecurity of raw TCP.

## Connection Lifecycle

A connection follows this sequence:

1. The client opens a TCP connection and completes a TLS 1.3 handshake
2. The client sends a **HELLO** message declaring its ATP version and optional metadata
3. The server responds with a **WELCOME** message confirming the version and server metadata
4. The client sends one or more **GET** or **INPUT** requests; the server responds with **PAGE**, **RESOURCE**, **REDIRECT**, or **ERROR**, as appropriate
5. The client may **SUBSCRIBE** to live regions; the server pushes **UPDATE** messages
6. Either side sends **BYE** to gracefully close the connection

```
Client                                    Server
  │                                         │
  │──── TLS Handshake ─────────────────────►│
  │◄─── TLS Established ───────────────────│
  │                                         │
  │──── HELLO (version, metadata) ─────────►│
  │◄─── WELCOME (version, server info) ─────│
  │                                         │
  │──── GET /path ─────────────────────────►│
  │◄─── PAGE (content) ────────────────────│
  │                                         │
  │──── INPUT (form data) ─────────────────►│
  │◄─── PAGE (response) ───────────────────│
  │                                         │
  │──── SUBSCRIBE /endpoint ───────────────►│
  │◄─── UPDATE (region content) ───────────│
  │◄─── UPDATE (region content) ───────────│
  │                                         │
  │──── BYE ───────────────────────────────►│
  │◄─── BYE ───────────────────────────────│
```

Connections are **persistent** — a single TLS connection serves multiple page requests to the same site, similar to HTTP/1.1 keep-alive.

## Message Format

Messages are length-prefixed frames with a binary header and a variable body:

```
┌──────────┬──────────┬──────────┬─────────────────────┐
│ Length   │ Type     │ Flags    │ Body                │
│ (4 bytes)│ (1 byte) │ (1 byte) │ (variable, UTF-8)   │
└──────────┴──────────┴──────────┴─────────────────────┘
```

- **Length**: Total frame size in bytes including header, big-endian uint32. Maximum frame size is 16 MiB.
- **Type**: Message type code (see below).
- **Flags**: Type-specific modifier bits.
- **Body**: UTF-8 encoded content for protocol messages and PAGE/UPDATE markup; RESOURCE bodies are raw binary bytes.

The 16 MiB value is only a protocol-wide framing ceiling. Conforming peers
must reject known message types against their narrower semantic limits after
reading the six-byte header and **before** allocating or reading the body:

| Direction | Message | Maximum body |
|-----------|---------|--------------|
| Client → server | HELLO, GET, SUBSCRIBE | 16 KiB |
| Client → server | INPUT | 64 KiB |
| Server → client | WELCOME, REDIRECT, ERROR | 16 KiB |
| Server → client | PAGE | 1 MiB AML plus 64 KiB session-metadata allowance |
| Server → client | UPDATE | 1 MiB |
| Server → client | RESOURCE | 512 KiB |

UNSUBSCRIBE, BYE, and server BYE have empty bodies. Message types sent in the
wrong direction and flag bits not defined for that message are protocol errors.

## Message Types

### Client to Server

| Type | Code | Purpose |
|------|------|---------|
| HELLO | `0x01` | Declare protocol version and optional client metadata |
| GET | `0x02` | Request a page by path |
| INPUT | `0x03` | Submit form data |
| SUBSCRIBE | `0x04` | Subscribe to live updates for a named region |
| UNSUBSCRIBE | `0x05` | Cancel a live subscription |
| PING | `0x06` | Prove the client is still present; resets the idle deadline |
| BYE | `0x0F` | Graceful disconnect |

### Server to Client

| Type | Code | Purpose |
|------|------|---------|
| WELCOME | `0x81` | Confirm protocol version and server metadata |
| PAGE | `0x82` | Deliver AML page content |
| UPDATE | `0x83` | Push new content for a live region |
| REDIRECT | `0x84` | Redirect to another URI (301 permanent, 302 temporary) |
| ERROR | `0x85` | Error response with numeric code and message |
| RESOURCE | `0x86` | Deliver a binary resource requested with GET (currently WASM) |
| PONG | `0x87` | Answer a PING |
| BYE | `0x8F` | Acknowledge disconnect |

## Client Metadata

HELLO/WELCOME establishes the ATP version and explicitly negotiates a
comma-separated `Capabilities` field. The server response may select only
capabilities offered by the client. Duplicate singleton fields, malformed
fields, unknown metadata, invalid UTF-8, and control characters are errors.

The implemented optional HELLO fields are `Terminal-Size`, `Color-Support`, and `Client`. The reference client currently sends only `Client`. WELCOME may include `Server` and `Site-Name`. Unknown fields are rejected.

ATP 0.2 peers require an exact `0.2` version match. This is a preview contract;
ATP/AML 1.0 remains unfrozen. Unoffered capabilities and behavior that depends
on an unnegotiated capability are protocol errors.

## GET Request

The GET body contains a path, with optional query parameters, referrer, and session token:

- The path is UTF-8, URL-encoded for special characters
- Query parameters are optional key-value pairs
- A Referrer field is parseable for forward compatibility, but the reference client does not send referrers
- If the client holds a session token whose scope matches the request path, it includes the token in a `Session` field (see Authentication below)

## PAGE Response

The PAGE body contains AML markup. See [03-markup.md](03-markup.md).

PAGE flags:
- **CACHEABLE** (`0x01`): Reserved cache hint. The reference client does not currently cache pages.
- **HAS_LIVE_REGIONS** (`0x02`): The page contains live-updating regions that the client should subscribe to
- **HAS_SESSION** (`0x08`): The body begins with session metadata before a blank line separator, then AML content (see Authentication below)

When `HAS_SESSION` is set, the frame body has this structure:

```
Set-Session: <token> <scope> [expires]
Clear-Session: <scope>

<AML content>
```

The metadata section contains `Set-Session` and/or `Clear-Session` directives, one per line, terminated by a blank line (`\n\n`). Everything after the blank line is the AML page content. When `HAS_SESSION` is not set, the entire body is AML content — this preserves backward compatibility with clients that do not support sessions.

## INPUT Submission

The INPUT body contains a target path and URL-encoded form field data. Repeated field names are permitted and retain document order. Like GET, if the client holds a matching session token for the target path, it includes it in a `Session` field. The server responds with a PAGE, REDIRECT, or ERROR.

## Binary Resources

A GET whose resolved path is an allowed `.wasm` file receives a RESOURCE response containing the raw module bytes. Resource requests do not carry query strings, referrers, or session tokens. The reference client and server both cap WASM resources at 512 KiB and cache them by canonical origin plus path.

## Live Updates

For real-time content like chat or live feeds, the client subscribes to a named region by sending SUBSCRIBE with an endpoint path, region identifier, an optional `Mode` header, and the path-scoped session token when one exists. The server resolves that token for the initial update and subsequent poll renders, then pushes UPDATE messages containing AML fragments. As with GET and INPUT, clients never attach sessions over plaintext transport.

The client sends UNSUBSCRIBE before navigating away from a page or re-subscribing, ensuring the server stops pushing stale updates.

An UPDATE body is limited to 1 MiB. A connection may hold at most 16 live subscriptions. UPDATEs may arrive while a request is outstanding; clients must retain them and continue waiting for the PAGE, RESOURCE, REDIRECT, or ERROR response. The reference client retains at most 64 such pending updates.

### Subscribe Modes

The SUBSCRIBE body supports a `Mode` header that controls how updates are delivered:

```
SUBSCRIBE /chat/stream
Region: chat
Mode: delta
Session: abc123
```

| Mode | Behavior |
|------|----------|
| (omitted) | Full replacement. Each UPDATE contains the complete region content. Default, backward compatible. |
| `delta` | Incremental. Each UPDATE contains only new content appended since the last update. The server tracks per-subscriber state and falls back to full replacement if the content was rewritten rather than appended. |

### UPDATE Flags

The UPDATE frame's flags byte carries update metadata:

| Bit | Name | Meaning |
|-----|------|---------|
| `0x01` | DELTA | This update contains incremental content, not a full replacement. Only set when the subscription uses delta mode and the server confirmed the content was appended (prefix unchanged). |

When the delta flag is not set, the client replaces the region's content entirely. When set, the client appends (or prepends, depending on scroll mode) the content to its existing buffer.

## Error Codes

| Code | Meaning |
|------|---------|
| 400 | Bad request |
| 401 | Authentication required (see Authentication below) |
| 403 | Forbidden (authenticated but not authorized) |
| 404 | Page not found |
| 429 | Rate limited or bounded resource exhausted |
| 500 | Server error |
| 503 | Server unavailable |

## Redirects

Redirects carry a status code (301 permanent or 302 temporary) and a target URI. The client follows redirects automatically up to a limit of 5. Cross-site redirects establish a fresh connection and HELLO/WELCOME exchange with the new server.

## Connection Management

- **Keep-alive**: Connections remain open after serving a page. The client reuses the connection for subsequent requests to the same site.
- **Idle timeout**: The reference static server closes connections after 30 seconds without a client frame. Server-initiated UPDATE traffic does not reset that deadline, because pushing to a socket is not evidence that anyone is reading it.
- **Keepalive**: PING is the cheap frame that supplies that evidence. The reference client sends one every 10 seconds, a third of the deadline, so two must be lost before a connection is dropped. PING and PONG carry no body, are legal while a request is outstanding, and are not capability-gated.
- **Requests**: A client sends one GET or INPUT at a time. UPDATE frames may be interleaved with its response.
- **Concurrent connections**: ATP does not mandate a client connection count. The reference client uses one connection for the current site.

## Authentication

Authentication in ATP is **optional, path-scoped, and strictly site-local**. Sites work without login by default. There is no cross-site session mechanism — session tokens are structurally confined to the site that issued them.

### Design Principles

- **Anonymous by default.** No page should require authentication unless it genuinely needs identity. Public content, read-only pages, and anonymous interactions work without login.
- **Path-scoped.** A site may have multiple authenticated sections with separate credentials. A session token for `/admin/` is never sent on requests to `/members/` or `/`. Each token is bound to an absolute path scope.
- **No cross-site facility.** Session tokens are keyed to a specific site domain. The client never sends a token to a different site. There is no mechanism for third-party sessions, token sharing, or federated identity at the protocol level.
- **Bounded and ephemeral.** The reference client keeps a small in-memory session store and discards it when the process exits.

### Login Flow

Authentication uses the existing form submission mechanism — no special login message type is needed:

1. The user navigates to a page that requires authentication
2. The server may respond with the page normally (if the section allows anonymous access) or with a 401 error whose body contains a login page with input fields
3. The user submits credentials via a normal INPUT message (username, password, or whatever the site requires)
4. If authentication succeeds, the server responds with a PAGE or REDIRECT and includes a `Set-Session` field in the response
5. The client stores the token, scoped to the declared absolute path on that site
6. Subsequent GET and INPUT requests whose path matches the scope automatically include the token

```
Client                                    Server
  │                                         │
  │──── GET /admin/dashboard ─────────────►│
  │◄─── ERROR 401 (login page AML) ───────│
  │                                         │
  │──── INPUT /admin/login ───────────────►│
  │     (username=alice&password=...)        │
  │◄─── PAGE + Set-Session ────────────────│
  │     (token=abc123, scope=/admin/)       │
  │                                         │
  │──── GET /admin/dashboard ─────────────►│
  │     Session: abc123                     │
  │◄─── PAGE (dashboard content) ──────────│
```

### Set-Session Field

The `Set-Session` field appears in PAGE responses. It contains:

- **Token**: An opaque string (the client stores it but does not interpret it). Maximum length 4,096 characters.
- **Scope**: An absolute path scope that matches only on complete path-segment boundaries. Defaults to `/` (the entire site) if omitted.
- **Expires**: Optional Unix timestamp after which the client discards the token. If omitted, the token remains active for the lifetime of the client process unless cleared or invalidated.

A server may issue multiple tokens with different scopes on the same site. For example, a site might issue one token scoped to `/admin/` and another scoped to `/members/`.

### Session Field on Requests

When the client sends a GET or INPUT request, it checks its stored sessions for that site. If one or more tokens match the request path on a complete path-segment boundary, the client sends the **most specific match** (longest matching scope) in the `Session` field.

For example, with stored tokens:
- Token A scoped to `/`
- Token B scoped to `/admin/`

A request to `/admin/users` sends Token B. A request to `/about` sends Token A. A request to `/public/` with no matching token sends no Session field.

### Clear-Session

The server can invalidate a session by including a `Clear-Session` field in a PAGE response, specifying the scope to clear. The client discards the matching stored token. This is used for logout.

### Client Storage

The reference client keeps sessions in memory for the current process, keyed by canonical site origin and scope path. It does not persist session tokens across restarts. The `:sessions` command displays the client-owned session inventory without exposing token values. Servers can revoke a token and instruct the client to clear it through the logout flow.

### Limits

| Limit | Value |
|-------|-------|
| Maximum token length | 4,096 characters |
| Maximum stored sessions per site | 8 |
| Maximum total stored sessions | 256 |
| Maximum scope path length | 1,024 characters |

## Certificate Management

ATP servers support three TLS modes:

### Manual Certificates

Operators provide a certificate and key file. Automated certificate provisioning and renewal are outside the reference server.

### Self-Signed Certificates (Development Only)

Tests may construct a self-signed listener through a hidden test helper. The production `dustnetd` CLI does not generate certificates. A client can connect to an operator-provided self-signed certificate only with `--insecure`, which disables certificate and hostname verification for that connection.

### Plaintext Mode (Development Only)

TLS can be disabled entirely for local development. In plaintext mode, the server binds only to localhost by default and prints a prominent security warning. Clients display an indicator when connected to a plaintext server and may refuse plaintext connections to non-loopback addresses.

## Extensibility

The protocol reserves message type codes `0x10`–`0x7F` for future client messages and `0x90`–`0xFF` for future server messages. ATP 0.2 treats an unknown message type as a protocol error; new message types therefore require a version or negotiated capability. Unknown HELLO/WELCOME metadata is rejected.

PING and PONG occupy the core code ranges rather than the reserved ranges above. They are part of 0.2 itself, not extensions to it — they were added while 0.2 was still unreleased, so no deployed peer ever saw a 0.2 without them, and no negotiation is needed to use them. An addition made after 0.2 ships would need the version or capability the paragraph above requires.

The normative ATP grammar, exhaustive connection-state table, AML lexical grammar, and mechanically checked implementation limits are in [05-conformance.md](05-conformance.md).

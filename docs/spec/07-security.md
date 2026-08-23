# Security Model

> Production-boundary note: `dustnetd` is a static server that installs no
> hooks — no include resolution, no form handling, no session resolution — so it
> authenticates nobody, generates no content, and refuses `INPUT` with 405. This
> document describes that boundary, plus the guarantees `dustnet-server` makes to
> a server that *does* install hooks: escaped generation, bounded submissions,
> and an identity boundary that keeps session tokens away from handlers.
>
> What a hooked server does above those guarantees — who may write, password
> storage, rate limiting — is its own, and is not described here.
>
> Accounts, email verification, password storage and plugin dispatch are
> provided by no crate here and described nowhere in this spec. A site that
> needs them builds them on the hooks, in its own repository.

## Threat Model

Dustnet is designed around a core reality: **users will connect to untrusted servers run by strangers**. Unlike SSH (where you trust the server because it's your machine) or HTTPS (where browsers present an enormous attack surface), the Dustnet reference client aims to remain minimal and auditable.

### Actors

Two sides, facing opposite directions. A client trusts nothing a server sends; a
server that accepts submissions trusts nothing a visitor sends.

| Actor | Trust Level | Faced by | Description |
|-------|-------------|----------|-------------|
| Client user | Trusted | — | The person running the ATP client |
| Site operator | Untrusted | Client | Runs an ATP server, serves content |
| Network observer | Untrusted | Both | Can observe or modify traffic (MITM) |
| Other users | Untrusted | Client | Users on the same or different sites |
| Form submitter | Untrusted | Server | Anyone who can reach the port and send an `INPUT` |
| Session holder | Untrusted | Server | Presents a token, possibly stolen, expired or invented |
| Site handler | Trusted for policy, not for correctness | Server | The site's own installed hooks |

### Trust boundary

Remote ATP servers, AML, WASM modules, redirects, live updates and protocol
sequences are assumed malicious. The client must preserve **origin isolation**,
**terminal integrity**, **bounded resource use** and **local confidentiality**
under malformed or adversarial input.

The operating system and terminal emulator are trusted. Compromised hosts and
malicious operator-installed server extensions are out of scope.

A server that installs hooks faces the other direction and must additionally
preserve **content integrity** — a page contains only markup the server itself
constructed — and **credential containment** — a session token reaches neither a
handler nor a page. `dustnetd` installs no hooks, so those adversaries cannot
reach it; the guarantees are listed because a site server built on
`dustnet-server` inherits them, and because a guarantee stated only in prose is
one nobody checks.

Security context is part of origin identity: verified TLS, insecure TLS and
loopback plaintext do not share sessions, subscriptions, pending updates or
cached resources.

The adversaries this model names, and the control and evidence answering each,
are data rather than prose:
[`verification/threat-model.json`](../../verification/threat-model.json). An
adversary with no evidence, or evidence naming a test that does not exist,
fails the build.

### Assets to Protect

1. **Client machine** — the only server-provided code that runs is WASM animation effects, confined to a draw-only sandbox (see below); no file access, no shell escape
2. **User privacy** — browsing history and activity stay local
3. **User attention** — resource limits prevent CPU and memory abuse
4. **Network integrity** — TLS prevents eavesdropping and tampering

## Core Security Principle: Content Is Declarative; Code Is Sandboxed

**AML — the content format — is not Turing-complete. There is no scripting language, no eval, no way for markup to branch, loop unboundedly, or reach a system resource.** Structure and text arrive as data, and the client's parser and layout engine are the only things that interpret them.

There is one deliberate exception: **WASM animation effects.** A page may reference a `.wasm` module (`[animation wasm src="..."]`) that the client fetches from the site and executes to produce procedural visuals. This *is* server-provided code running on the client, so the security guarantee is not "no code runs" but "the only code that runs is confined to a sandbox that can do nothing but draw":

- The module executes in an interpreter (`wasmi`, no JIT) with **fuel metering** — it is preempted/stopped if it exceeds a per-tick instruction budget, so it cannot hang the client.
- Its linear memory is **capped**; growth beyond the limit terminates the instance rather than the client.
- The **host surface is six scalar-only draw operations**. The module cannot open files, sockets, or read the client's memory; the host never dereferences guest pointers.
- A module that traps, exhausts fuel, or hits the memory cap **kills only itself** — the page keeps rendering.

What remains true for *every* server, with or without WASM:
- A malicious server cannot access the client's filesystem, network, clipboard, or any system resource.
- A malicious server cannot fingerprint the user beyond the small metadata surface declared in HELLO.
- A malicious page cannot interact with other pages or sites.

The attack surface is therefore the AML parser, the rendering engine, **and the WASM sandbox** — all three are resource-bounded, and the parser and scanner are fuzz-tested.

### AML Animations Are Not Computation

AML's own (non-WASM) animations are finite state machines with pre-declared frames and bounded loop counts. They cannot branch conditionally, read input, modify their own state based on runtime conditions, or communicate with the server. An AML animation is a tape that plays forward — it is not computation. WASM effects (above) are the only computational content, and they are sandboxed accordingly.

## Client-Side Security

### Terminal Escape Sequence Injection

**Threat**: A malicious server embeds raw ANSI escape sequences in content to exploit terminal emulator vulnerabilities — cursor repositioning, title bar injection, clipboard access via OSC 52, and similar attacks.

**Mitigation**: AML and client-owned chrome are separate paths to terminal output, and both sanitize untrusted text. The AML parser operates on a whitelist basis:
1. It parses AML tags and text content
2. It strips all bytes outside the printable Unicode range (except explicitly allowed control characters like newline within `[pre]`)
3. The byte `0x1B` (ESC) is always stripped
4. The renderer generates its own escape sequences based on parsed attributes
5. Protocol errors shown in client chrome pass through the same control-character sanitizer and grapheme-safe, display-width truncation
6. Only client-generated escape sequences reach the terminal

A server-sent escape sequence embedded in text content is stripped before it can reach the terminal.

**Deceptive formatting.** Escape stripping answers what the *terminal* will
execute. It does not answer what the *reader* will believe, and those are
different threats. Bidirectional controls and invisible formatting characters
sit in the printable range the whitelist admits, and a conforming terminal
renders them exactly as Unicode specifies — the deception is in the eye, not
the emulator.

The same sanitizer therefore also strips:

- Bidirectional marks and the Arabic letter mark (U+061C, U+200E, U+200F)
- Explicit bidi embedding and override (U+202A–U+202E), of which U+202E
  RIGHT-TO-LEFT OVERRIDE is the Trojan Source primitive: it lets a link label
  render as one URI while the link carries another
- Bidi isolates (U+2066–U+2069)
- Zero-width space, word joiner and the invisible math operators (U+200B,
  U+2060–U+2064), which let two labels differing in bytes render identically
- Interlinear annotation (U+FFF9–U+FFFB), which hides the text it brackets
- Zero-width no-break space / byte-order mark (U+FEFF)

Two families are deliberately preserved, because removing them corrupts
legitimate text rather than protecting anyone: U+200C ZERO WIDTH NON-JOINER
and U+200D ZERO WIDTH JOINER, which compose emoji sequences and drive Arabic
and Indic shaping; and the U+E0000 tag block, which emoji flag sequences are
built from. A client that renders these must not treat their presence as
evidence that a label is trustworthy.

Deceptive formatting is removed rather than rejected, so a hostile page renders
with the deception gone instead of denying the user the rest of the document.

Hostnames are a separate and stricter case: the URI parser rejects any
non-ASCII host outright, so the IDN homograph vector does not reach the
origin comparison at all.

### Resource Exhaustion

**Threat**: A malicious server sends a page designed to consume excessive CPU, memory, or bandwidth.

**Mitigations**:

| Attack Vector | Defense |
|---------------|---------|
| Huge page | PAGE length rejected before allocation; 1 MiB AML maximum plus bounded session metadata |
| Huge request | Control messages capped at 16 KiB and INPUT at 64 KiB before allocation |
| Deep nesting | 32 levels maximum |
| Many elements | 10,000 maximum |
| Many animations | 16 regions, 256 total frames |
| High framerate | 30 FPS maximum |
| Large art blocks | 200 x 200 maximum |
| Oversized layout geometry | 512 explicit columns, 4,096 row coordinates, 2,048-cell element dimensions, and 1,048,576 cells per buffer |
| Resource cache flooding | 32 entries and 8 MiB aggregate, LRU eviction |
| History retention | 128 entries and 16 MiB retained AML, oldest-first eviction |
| Live region spam | 1 MiB per complete UPDATE body, 64 entries and 8 MiB client-queued, 16 subscriptions per connection, and 128 MiB retained subscription memory server-wide |
| Infinite redirects | 5 redirects maximum |
| Slow response | 10 second timeout |
| Connection flood | 2048 server-wide connections, 4 per source IP, both operator-configurable |
| WASM CPU abuse | Fuel metering (instruction budget per tick) |
| WASM size | 512 KiB maximum module size |
| WASM memory abuse | One 4 MiB linear memory per guest, at most 16 guests (64 MiB aggregate), and at most 4 tables of 10,000 references each |

### Information Leakage

**Threat**: A malicious server fingerprints users or tracks them across sites.

**Mitigations**:
- HELLO is the only automatic client metadata. The reference client currently sends its client name/version, not terminal dimensions, fonts, plugins, GPU, or canvas-like data
- Session tokens are strictly site-scoped — there is no cross-site session mechanism, no third-party tokens, and no way for one site to read or influence another site's sessions (see Session Security below)
- No referrer leakage. The reference client does not send the optional Referrer field
- Custom transitions fetched from remote sites do not include client information beyond a normal GET request

### Link and URI Attacks

**Threat**: Malicious links exploit the client or confuse the user.

**Mitigations**:
- Only `atp://` URIs are navigable. No `file://`, `http://`, `ssh://`, or other scheme
- The client displays the target URI before navigation (in a status bar or on focus)
- URI length limit: 2,048 characters
- Hostnames are canonicalized to lowercase; malformed authorities, unbracketed IPv6 literals, controls, fragments, and invalid ports are rejected
- A PAGE's `Path` may relabel the location only within the origin that sent it. It is a path, not a URI: a value carrying a scheme, beginning `//`, or containing a fragment is a malformed body, so a page cannot use the field to move the client to another site. Moving between sites remains REDIRECT's job, which the client counts against the redirect limit and, across origins, performs with a fresh connection and HELLO. A `Path` that cannot be used is ignored rather than fatal, so a malformed one cannot suppress a page either.

## Server-Side Security

`dustnetd` accepts nothing: it installs no include resolver, no input handler and
no session resolver, so it generates no content, writes nothing, and refuses
`INPUT` with 405. Everything in this section past Site Isolation therefore
describes what `dustnet-server` guarantees to a server that *has* installed
hooks — a site server, of which `dustnet-sites/dustnews` is the worked example.

### What the library guarantees, and what it does not

The split matters, because a site that assumes the wrong half is insecure in a
way that reads as secure.

**Guaranteed by the library.** Generated content cannot become markup, because a
handler returns tokens and one escaping serializer writes every bracket. A
submission is bounded in bytes and in field count before a handler sees it. A
resolved page over the page limit is refused rather than truncated. A resolver
cannot make the server loop by returning another placeholder. A session token is
resolved once, at the boundary, and handlers are handed an identity instead.

**Not guaranteed, and not knowable by a generic ATP server.** Who may write.
Whether a name is already taken. How often one source may try a password. What a
field means, how long it may be, and whether it is allowed to contain a tab. How
passwords are stored. Whether the same account may vote twice. All of that is the
site's, and a site that skips it has an open write endpoint whatever the library
does.

Two failure modes worth naming because they are invisible until they bite:

- **A hidden form is not a control.** A renderer that shows a comment box only to
  signed-in readers has decided presentation. The handler must refuse the same
  action independently, because a hand-made `INPUT` never came from a form.
- **Escaping on the way out is not sanitising on the way in.** The serializer
  stops a submitted tab becoming markup. It does nothing about that tab splitting
  a row in a tab-separated store — a storage-format concern, fixed where the
  value is written, and one no amount of AML escaping addresses.

### Site Isolation

Sites are independent servers. One compromised site cannot affect others. There is no shared runtime, database, or process space. Each site should run in its own container or VM with standard server hardening.

### Input Validation

`dustnetd` rejects `INPUT` with status 405, and
`without_a_handler_input_is_still_refused` holds it to that.

The `dustnet-server` library exposes three optional hooks — an include resolver,
an input handler, and a session resolver. A server that installs none behaves
exactly as one with no hooks at all, which is what `dustnetd` does and what
`without_a_resolver_includes_are_served_verbatim` asserts. A server that installs
them accepts form submissions, and is then responsible for validating,
sanitizing and rate-limiting everything it accepts: **client-side AML limits are
not a server-side security boundary**, and a `maxlen` on a form is a hint to a
cooperating client, not a constraint on a hand-made `INPUT`.

The library bounds what it hands such a server: an `INPUT` body is capped by
`MAX_INPUT_MESSAGE_SIZE` and the parsed field vector by `MAX_FIELDS`, so the cost
of one submission is bounded in both bytes and elements before a handler sees it.
Everything above that — who may write, how often, and what a field may contain —
is the handler's, because only the handler knows what the site means.

### AML Injection

**Threat**: User input is echoed into AML content without escaping, allowing one user to inject AML tags into pages seen by other users (analogous to XSS in web browsers).

**Mitigation**: Server-generated content is composed as **tokens**, not text.
`dustnet_core::serialize::to_aml` is the only thing that writes a bracket, and it
escapes as it writes, so user-submitted content placed in a `Token::Text` cannot
become markup regardless of what it contains. The property is stated as
`scan(to_aml(tokens)) == tokens` and checked three ways: unit tests per escaping
context, the round trip over every AML document in the repository, and the
`fuzz_serialize` target.

Escaping is not one function. Text content and quoted attribute values have
different rules, and a URL is a third case where escaping is *insufficient* — a
perfectly escaped hostile URL is still a phishing link, so scheme checking is a
semantic control that belongs where a URL enters the system.

A server that instead builds AML by concatenating strings has to remember an
escape at every interpolation site, and the failure mode is not hypothetical: an
earlier prototype of this project escaped a link's title, author and domain and
then interpolated the submitted URL raw on the next line.
`dustnetd` generates no AML at all, having no resolver installed.

### Session Security

The client-side guarantees in this section apply to ATP session directives
regardless of server implementation. `dustnetd` neither issues nor validates
tokens, having no session resolver installed.

A server that installs one gets a boundary rather than a convention: the library
resolves a presented token to an identity *once*, and hands the include resolver
and input handler that identity and **never the token**. A token is a bearer
credential and a username is not — a handler holding a token could replay it, log
it, or embed it in a page it generates, and a token in a page is a
session-stealing bug that looks like a rendering bug.
`handlers_receive_an_identity_and_never_the_token` asserts it end to end.

Two consequences of that boundary are worth stating because they are not
obvious:

- An unrecognised token is indistinguishable from no token. Both resolve to
  anonymous, because a caller able to tell them apart would be an oracle for
  whether a token had ever existed.
- A handler cannot end its own session, since it does not know the token naming
  it. Revocation therefore runs through `SessionResolver::revoke`, called by the
  server when a handler's outcome clears the session — which ends exactly the
  session that was presented rather than every session the person holds.

Storing, expiring and revoking sessions remain the resolver's responsibility.
Expiry has to be enforced there: the expiry sent to a client lets it tidy up,
but a client that ignores it must still be refused.

**Threat**: Session tokens are used for cross-site tracking, session hijacking, or privilege escalation across scopes within a site.

**Mitigations**:

- **No cross-site sessions.** Session tokens are keyed to a specific site domain. The client never sends a token to a different site. There is no third-party session concept, no token sharing protocol, and no federated identity at the protocol level. This is a structural guarantee — the client has no mechanism to do it, not just a policy against it.
- **Path-scoped tokens.** Tokens are bound to an absolute path and match only on complete segment boundaries. A token scoped to `/admin/` is never sent on requests to `/members/` or `/`. The client sends only the most specific matching token per request. This enforces least-privilege: a compromised token for one section of a site does not grant access to other sections.
- **No token in HELLO.** Session tokens are established after the handshake and apply per-request, not per-connection.
- **Server-side validation.** The server maintains an authoritative session store. Tokens are validated on every request by looking them up in the server-side map. Expired or revoked tokens are rejected immediately — there is no window where a stale client token grants access.
- **Server-side invalidation.** Session-aware servers can clear sessions by
  sending a clear-session directive. A correct server also destroys its own
  session record, so the token stops being valid even if the client ignores
  the directive.
- **CSPRNG tokens.** Session tokens are 32 bytes of cryptographic randomness (via `getrandom`), hex-encoded. They cannot be predicted, enumerated, or derived from user information.
- **No token values exposed locally.** The `:sessions` inventory displays
  scopes and expiry without ever showing a token, and names whether sessions
  are being remembered so that whether closing the client is a logout is not
  something the user has to infer.
- **Persistence narrowed at rest.** A CA-verified session outlives the process
  by default, written to `$XDG_STATE_HOME/dustnet/sessions`;
  `remember-sessions = false` in `client.conf` restores memory-only sessions.
  This is the one place the client keeps a credential at rest, and it is worth
  being explicit about the trade: a stored token is a token an attacker with
  read access to the user's account can take, which the ephemeral-only client
  did not offer. What is stored in exchange is the site's own revocable,
  expiring, path-scoped token and never a password, and the alternative cost —
  re-authenticating on every launch — is paid continuously by every user while
  appearing in no threat model. Persistence cannot widen what a token reaches;
  it narrows what is eligible to be stored at all:
  - Only `verified-tls` origins are written. A `--tofu`, `--insecure` or
    plaintext session stays in memory and dies with the process. The file
    therefore carries no security label — there is no field to alter that
    would promote a stored token into a stronger origin partition, because
    every line is read back at the one level that was allowed to be written.
  - Only tokens carrying an expiry are written, and an expired line is
    discarded on load rather than sent. A token with no expiry is a
    credential with no end, and the file is not the place to keep one.
  - The file is created owner-only (`0600`) and refused on load when other
    users can read it. A pin is only worth the integrity of the file holding
    it; a session token is only worth its confidentiality, so the check runs
    in the other direction from the pin store's.
  - Loading admits through the same path a server directive does, so the
    per-site and total bounds and the client's memory accounting apply to a
    file exactly as they apply to a live response.
  - A failed write leaves the session working in memory: unlike a pin, a
    session that does not persist is an inconvenience rather than a false
    claim about who the peer is. A failed *clear* is the opposite, and removes
    the file outright — a logout must not leave the token at rest.
- **Bounded storage.** Maximum 8 sessions per site, 256 total. Maximum token length 4,096 characters. These limits prevent a malicious site from flooding the client's session store.
- **TLS-only transport.** Session tokens travel over TLS 1.3 and are never sent in plaintext. The client refuses to attach session tokens to requests over plaintext connections, even in development mode. This is enforced structurally — the session lookup returns no token when the connection is unencrypted.

## Transport Security

### TLS Requirements

- Minimum version: TLS 1.3 (TLS 1.2 and older are refused by the handshake — the client and server both pin `TLS13` as the only protocol version)
- Standard X.509 certificate validation against the system/webpki root store
- Self-signed certificates are rejected by default

### Development / Self-Signed Certificates

Public sites are expected to present CA-signed certificates, validated normally. A site with no authority behind it has two further routes, and they are not equivalent.

**The trust prompt.** When `dustnet connect` reaches a site whose certificate it cannot verify — self-signed, expired, or issued for another name — and nothing is pinned for that host and port, it stops and asks. The prompt names the site, states why verification failed, and shows the certificate's full SHA-256 fingerprint so it can be compared against one the operator published. Accepting pins that exact certificate; declining connects to nothing. There is no third option and no way to proceed unpinned, because "continue without deciding" is the state that produces users who have no idea what they are talking to.

Answering the prompt changes the site's security context, so the navigation that provoked it is re-issued rather than resumed: the pinned origin is a different origin, and reusing state computed before the decision would cross a trust boundary the partition exists to enforce.

**A pinned certificate that changes is a hard failure, never a prompt.** By that point the only explanations are a re-keyed server and an interception, and the client cannot tell them apart, so it refuses and says so — naming both fingerprints and what to do about each. Asking again here would train users to click through the one moment an interception is visible.

A pin is honoured on later connections without any flag, because it records a decision the user already made. Creating one always requires either the prompt or `--tofu`, so the presence of a store can never downgrade a connection that was not deliberately pinned.

**`--tofu`** pins the first certificate seen without asking. It is the non-interactive form of the prompt, for scripts and for callers with no terminal; a library caller that never prompts receives an error naming the fingerprint and the reason, and connects to nothing.

Pinning skips **host name** verification and nothing else. The pin binds a certificate to the authority the user typed, which is the same binding SSH makes and the reason a self-signed certificate with no matching SAN is usable at all. The handshake signature is verified against the pinned certificate as usual: a certificate is public, so matching a fingerprint without checking the signature would authenticate anyone who had ever connected to the site rather than the party holding its private key.

Pins live in `~/.config/dustnet/known_sites`, one per line, and the client refuses to use the file if other users can write to it — an attacker who can add a line chooses the certificate the client will accept. `dustnet trust list` and `dustnet trust forget <host>` manage it, and deleting a line by hand does the same thing.

**`--ca-file` — a private authority.** Adds PEM anchors to the built-in bundle rather than replacing it, with host name verification unchanged. The right choice when an organisation runs its own CA.

**`--insecure` — no verification.** Disables certificate and host name checking entirely, for every connection it is passed and with nothing carried forward. This is a development escape hatch, not a trust model: such a connection is encrypted but unauthenticated and offers no protection against an active man-in-the-middle. `--tofu` is equally convenient on the first connection and authenticates every one after it, so `--insecure` should be reached for only when the goal is explicitly to *not* check.

Each level is part of origin identity, so state learned under one is never reused by another: a session established over `--insecure`, a pinned session and a CA-verified session to the same host and port are three separate partitions. The status bar names the level of the connection in hand — `no-tls`, `insecure`, or `pinned` — and is empty only for ordinary CA-verified TLS. It is read from the live origin rather than from the launch flags, so navigating between differently trusted sites changes what it says.

## Abuse Prevention

### Rate Limiting

Production `StaticServer` caps global connections, connections per source IP,
subscriptions per connection, and aggregate retained subscription memory.
The reference client deliberately does not impose an arbitrary GET
rate: navigation is serialized and bounded by response deadlines.

### Content Moderation

Moderation is per-site — site operators moderate their own content. Client-side site blocking is not currently implemented.

## Security Audit Checklist

For client implementations:

- AML parser rejects all raw escape sequences
- AML parser enforces all document limits
- Parser uses a whitelist for allowed characters in text content
- Bidirectional controls and invisible formatting characters are stripped from text
- Color and style attributes are validated (no injection into escape sequences)
- URI scheme restricted to `atp://` and relative paths only
- Redirect loop detection (maximum 5)
- TLS 1.3 minimum enforced (protocol version pinned; 1.2 refused)
- Certificate validation against CA roots by default; `--tofu` pins per host and port and verifies the handshake signature against the pinned certificate; `--insecure` (dev-only) disables validation entirely
- The status bar names any transport weaker than CA-verified TLS
- The pin store is refused when it is writable by other users
- Input field values are not leaked cross-site
- Session tokens are never sent cross-site
- Session tokens are never sent over plaintext connections
- Session scopes are absolute, traversal-free paths and match complete path segments
- Session storage limits enforced (8 per site, 256 total), on a loaded session file as well as on a server directive
- `remember-sessions = false` keeps sessions in memory for the whole run and
  writes nothing
- Only CA-verified origins are ever persisted, and only with an expiry
- The session store is created owner-only and refused when it is readable by other users
- Custom transition data is validated against limits
- Live region update bodies, queued updates, and active subscriptions are bounded
- Memory bounded: page content, animation frames, live buffers all capped
- WASM execution is fuel-metered with strict budgets
- Fuzz testing of AML parser with malformed and adversarial input

For `dustnetd`:

- Static-root containment and regular-file checks are enforced
- `INPUT` is rejected, and no resolver, handler or session hook is installed, so
  no code generates content and nothing is written
- Global, per-IP connection, and per-connection subscription limits are enforced
- Connections observe shutdown and leave voluntarily, so the drain deadline is
  a backstop rather than the ordinary path
- TLS is restricted to TLS 1.3; plaintext bind is loopback-only
- Frame reads, writes, and connection draining have bounded deadlines

A separate dynamic server implementation, outside Dustnet's supported
production boundary, is the site's own to secure: this section describes only
what the hooks themselves guarantee.

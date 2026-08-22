# Changelog

## 0.2.1 - 2026-08-22

### Fixed

- Panel entrance transitions animate in release builds. A `[state]` carrying
  `transition="draw-out"` and friends built its adapter, costed it against the
  governor, and then discarded it, so every authored box appeared fully formed
  instead of drawing itself in. The install was written as the argument of a
  `debug_assert!`, which does not evaluate its argument once assertions are
  compiled out -- so the effect was invisible in the debug profile the test gate
  runs, and only the shipped binary was affected. The page-level transitions a
  `[link transition=...]` names were never installed this way and always worked,
  which is what made the difference look like a property of the transition kind.
- `:sessions clear` clears sessions in release builds. The directive was applied
  to the store inside a `debug_assert!` -- the same mistake, in a second place --
  so with assertions compiled out the sessions stayed in memory and
  `persist_after_clear` then wrote them back to the store file the clear was
  meant to empty. 0.2.0 shipped that path and its changelog entry claiming
  `:sessions clear` deletes the file; on a release binary neither happened.

## 0.2.0 - 2026-08-22

The first release of the 0.2 line that is not a pre-release, so
`cargo install dustnet --locked` finds it without being told a version — a
`*` requirement never matches a pre-release, which is why every earlier install
line had to name one. Nothing was yanked to get here: crates.io went from
0.2.0-alpha.2 straight to this, because 0.2.0-alpha.3 was committed in git and
described below but never published to the registry — so the install line the
README carried for it never worked.

Everything alpha.3 got wrong about staying logged in. A login was accepted and
then rendered as anonymous, a reload returned to the form, and closing the
client threw the session away — three separate causes in three layers, each
enough on its own to make a working login look broken.

### Added

- Session persistence, in `~/.local/state/dustnet/sessions`
  (`DUSTNET_SESSION_STORE` names another path). What may be stored is narrowed
  rather than widened — CA-verified sites only, tokens with an expiry only, the
  file created `0600` and refused when other users can read it, and a loaded
  file admitted under the same 8-per-site and 256-total bounds a server's
  directives are. A password is never stored under any setting. `:sessions`
  says which mode is in effect, and `:sessions clear` now deletes the file as
  well as the in-memory sessions.
- `page-path`, a capability under which a PAGE names the path it represents.
  A submission is answered with the page its handler chose — a login submitted
  to `/login` is answered with the front page — and until now the client kept
  the submitted path as its location, so a reload after a login returned to the
  form. REDIRECT cannot stand in: `validate_redirect_body` forbids metadata, so
  a redirect cannot also issue the session a login grants. The field names an
  absolute same-origin path with an optional query and never a URI — a value
  carrying a scheme, beginning `//`, or containing a fragment is a malformed
  body, since each would turn a relabelling into a cross-origin navigation that
  skipped the redirect limit and the fresh HELLO a real redirect performs. A
  `Path` that cannot be used is ignored rather than fatal.

### Changed

- A login now survives restarting the client, where through alpha.3 closing it
  was a logout. `docs/spec/07-security.md` claimed ephemeral client storage
  unconditionally and now claims it for `remember-sessions = false`, which
  restores the old behaviour exactly. The trade is stated there rather than
  implied: a stored token is one an attacker with read access to the account
  can take, against a re-authentication cost every user paid on every launch.

### Fixed

- A link to a foreign scheme is refused instead of being resolved as a path.
  `resolve` special-cased `atp://` and treated anything else not `/`-rooted as
  a relative reference, so an `https://example.com` href became
  `atp://current-host/https://example.com` — a nonsensical request, and one
  that told the current site which external link its reader had clicked. RFC
  3986's rule decides it now, by the position of the colon relative to the
  first slash, which also catches `mailto:someone` while leaving
  `sub/odd:name.aml` a path. Activating such a link used to fail silently,
  which made Enter look like a dead key; it now says which scheme it refused.
- A response to a submission is rendered for the identity the handler granted.
  `serve_input` resolved the token the request *arrived* with, which at login
  time is nothing, so the frame that issued a session also drew the logged-out
  page. The granted token goes back through `SessionResolver` — the same
  chokepoint a GET uses — so nobody's word is taken for it, and a handler
  claiming a session the resolver rejects still renders anonymously.
- A refusal reason is bounded to 200 characters. A handler put a multi-line
  SMTP diagnostic into a refusal, that string became the ERROR body, and the
  client refused to lay it out — so the real failure was hidden behind the
  rendering failure it caused. Trimmed at the boundary rather than in each
  handler, and logged in full first so an operator keeps the diagnostic.
- The capability gate checks every flag a PAGE sets, not the first that
  matched. It was one guarded match arm per flag, so a PAGE claiming live
  regions *and* a session was admitted on `live-updates` alone.
- The two readers of the PAGE metadata block agree. `decode_body` and
  `validate_page_body` read one wire format on two paths, and
  `page_validation_matches_decoding` now compares them across a corpus crossed
  with all sixteen flag combinations rather than trusting a comment. Both also
  refuse a flag whose field is absent, so a message has one encoding.

## 0.2.0-alpha.3 - 2026-08-22

### Breaking

- `include` is now a built-in AML element name, so a component cannot be called
  `include` any more. A `[def name="include"]` that used to expand is now a
  diagnostic. Nothing else in the element catalogue changed, and every other
  document that parsed under alpha.2 still parses.
- An `[include]` inside a `[def]` body is refused with the new diagnostic
  `E052`. A component body is copied into every call site with `$attr`
  substitution applied, and server-generated content is substituted later, so an
  include expanded there would place generated content in a region where `$` is
  still live. The two mechanisms are kept disjoint rather than made to interact
  safely.

### Added

- `[include name=... /]`, a placeholder a server fills before a page goes on the
  wire. The parser validates it and `dustnet check` sees it; a client renders an
  unresolved one as nothing, deliberately not as its own literal text. See
  `docs/spec/03-markup.md`.
- `dustnet_core::serialize::to_aml`, which turns a token stream into AML and
  escapes as it writes. It exists so a server generating content around
  user-submitted data never composes markup by concatenating strings: text goes
  in a `Token::Text` and cannot become an element, whatever it contains. The
  property is `scan(to_aml(tokens)) == tokens`, checked by unit tests per
  escaping context, a round trip over every AML document in the repository, and
  the new `fuzz_serialize` target.
- Three optional hooks on `dustnet-server`, installed through
  `StaticServerConfig`: `with_include_resolver` fills placeholders,
  `with_input_handler` accepts form submissions, and `with_session_resolver`
  turns a session token into an identity. A server that installs an input
  handler answers `INPUT` instead of refusing it with 405.
- `SessionChange::token()`, so a caller can read back the session it just
  issued rather than threading the token through its own return values.
- A PAGE may name the path it is, under the new `page-path` capability: `Path:`
  in the metadata block, an absolute same-origin path with an optional query.
  The answer to a submission is often a different page from the one submitted
  to, and this is how a client learns that — a login submitted to `/login` is
  answered with the front page and now *lands* there, rather than displaying it
  under `/login` and reloading back into the form. Resolved against the URI that
  produced it and refused if it carries a scheme, a `//` prefix or a fragment,
  so it can relabel a location within a site but never move between sites; that
  stays REDIRECT's job, with the redirect limit and fresh HELLO that implies. A
  client that did not offer the capability gets the body it always got.

### Changed

- `dustnetd` is unchanged. It installs none of the hooks above, so it still
  authenticates nobody, generates no content, and refuses `INPUT` with 405 —
  held to that by `without_a_handler_input_is_still_refused` and
  `without_a_resolver_includes_are_served_verbatim` rather than by intent.
  `docs/adr/0001-production-boundary.md` records why the boundary moved by three
  traits and what stayed outside it.
- Layout is no longer exponential in `[box]` nesting depth. A `Fit` box measured
  its children into a throwaway buffer and then laid the same children out
  again, which was two subtree walks per level; the measurement is now memoised
  per node per layout pass. At the conforming maximum depth this was about
  twenty minutes and is now under two milliseconds. Closes the only entry in
  `verification/BUGS.md`, and `layout_at_max_depth_completes_within_time_bound`
  is cited from the threat model in place of a test that only proved deeper
  documents were rejected.
- The threat model covers the server side. `verification/threat-model.json`
  names three adversaries a server with hooks installed faces — a form
  submitter, a session holder, and the site's own handlers — under two new
  properties, content integrity and credential containment. Several statements
  in `docs/spec/07-security.md` that were true of the library and are now true
  only of `dustnetd` were corrected rather than left standing.
- Fuzz campaign rows are keyed on a fingerprint of the fuzzed code rather than
  on the workspace version, so a release cannot ship sources no campaign has
  covered and a version bump no longer invalidates a campaign that is still
  valid.

### Fixed

- A PAGE that sets several flags is checked against the capability each one
  needs. The check was a guarded match arm per flag and only the first matching
  arm ran, so a PAGE claiming live regions *and* a session was admitted on
  `live-updates` alone.
- A login answers with the logged-in page. The response to an accepted `INPUT`
  is rendered for the session the handler just granted rather than the one the
  request arrived with, so a login no longer returns `Set-Session` and a
  logged-out nav bar in the same frame — which read to the person who had just
  typed their password as a login that failed. The granted token is resolved
  through `SessionResolver` exactly as a `GET` would resolve it, so a handler
  still cannot render a page as someone by claiming to have authenticated them:
  a token the resolver rejects renders anonymously.
- A session token never reaches a handler. The library resolves it to an
  identity once, at the boundary, and hands handlers the identity — a token in a
  generated page is a session-stealing bug that looks like a rendering bug.
  Revocation runs through `SessionResolver::revoke`, called by the server, so a
  handler that cannot name its own session still ends exactly the one that was
  presented.

## 0.2.0-alpha.2 - 2026-08-21

### Changed

- Relicensed to MIT alone, from `MIT OR Apache-2.0`. What is given up is
  Apache-2.0's explicit patent grant; what is kept is GPLv2 compatibility and a
  licence most readers already know. 0.2.0-alpha.1 was published under the dual
  grant and is yanked rather than deleted: deleting a crate blocks republishing
  the name for 24 hours, and the dual grant is the more permissive of the two,
  so nothing that resolved against it loses a right.

## 0.2.0-alpha.1 - 2026-08-21

### Breaking

- Added `PING` (`0x06`) and `PONG` (`0x87`) to the ATP message set. Both carry
  an empty body, are legal while a request is outstanding, and are not
  capability-gated. The protocol version stays `0.2`: these are core messages in
  a specification that has not shipped, not extensions to one that has, so they
  occupy the core code ranges rather than the reserved extension ranges
  described under Extensibility in `docs/spec/02-protocol.md`. A peer built against
  an earlier `0.2` will reject them, which is why this is recorded as breaking.
- Removed the root façade crate and the `dustnet serve` command. Dustnet is now
  a five-package virtual workspace: import `dustnet_core`, `dustnet_client` or
  `dustnet_server` directly, and replace `dustnet serve ...` with `dustnetd
  <site-directory> --cert ... --key ...`. Plaintext development serving requires
  `--plaintext-loopback` and cannot bind a non-loopback address.
- The supported server is static and plugin-free. Authentication, boards, chat,
  email, links, statistics and custom handlers moved to
  `examples/unsupported-social` and are excluded from production builds.
  Construct servers through `StaticServerConfig::bind_tls` or
  `StaticServerConfig::bind_plaintext_loopback`, then pass the configuration to
  `StaticServer::new`.
- `Origin` is the site key. It carries canonical host, explicit port and
  transport security; string host keys are not compatible.
- Removed the public raw client and all self-scoping compatibility APIs. Viewer
  lifecycle identities are opaque, reducer-issued values. `dustnet_client::Client`,
  the public `client` module, direct transport response types, the self-scoping
  fetch/submit/subscription/resource helpers and the animation network
  constructor were removed without replacement: the supported high-level network
  entry point is `run_connected_viewer`, and local rendering continues through
  `run_viewer` with explicit local assets. Drive async client/viewer integrations
  through `ViewerModel::reduce` and its `ViewerEvent`/`ViewerEffect` contract.
- `AtpUri` fields are read-only through accessors; duplication uses the fallible
  `AtpUri::try_clone`.
- Concrete animation adapters take owned identifiers and payloads; WASM adapter
  construction is fallible via `WasmAnimationAdapter::try_new`.
- Several error payloads changed shape as part of making diagnostics
  allocation-free (`ColorParseError` syntax variants are payload-free; dynamic
  protocol error payloads expose `Cow<'static, str>`).
- `CellBuffer::diff` returns `Option<Vec<(u16, u16)>>`. It counts the differing
  cells before recording any, so its result is allocated once at its final size
  and an allocator refusal is reported rather than aborting.

### Added

- The plugin-free static `dustnetd` production server.
- Typed origins carrying canonical host, explicit port, and transport security,
  with sessions and resources partitioned by security context.
- Explicit ATP/AML 0.2 capability negotiation.
- `--max-connections` and `--max-connections-per-ip` on `dustnetd`, with
  `StaticServerConfig::with_connection_limits` promoted to public API. The right
  ceiling depends on the operator's `RLIMIT_NOFILE`, which the process cannot
  raise for itself under `forbid(unsafe_code)`; the requirement is documented in
  `docs/guides/production-support.md` and the effective ceilings are logged at startup.
- Server logging for the connection lifecycle. Accepting, closing, handshake
  failures and handshake timeouts are logged for every connection rather than
  only for failing ones, so the log answers "is anyone connected?" and not just
  "why can nobody connect?". Per-request detail — the negotiated version and
  capabilities, each GET and its 404s, SUBSCRIBE and UNSUBSCRIBE — sits at
  `debug`, which keeps `make serve` readable while making `RUST_LOG=debug`
  worth turning on.
- `StaticServer::subscription_memory`, reporting retained live-region bytes
  against the server-wide ceiling. Budget exhaustion presents to a user as a
  live region that quietly stops updating; every refusal is now logged and the
  pressure is observable before it becomes one.
- A viewer keepalive. The client sends `PING` every 10 seconds against the
  server's 30-second idle deadline, so a reader who is not clicking keeps one
  connection instead of being disconnected and redialling TLS roughly twice a
  minute. The keepalive is deliberately independent of whether the page has live
  regions: an idle connection is the one at risk.

- `tests/conformance/` is executable. `dustnet-core`'s `conformance_vectors`
  test runs every published vector on every `cargo test`, and fails if a
  fixture on disk has no manifest entry or an entry names a file that is gone.
  The `.atp` vectors were previously read by nothing, and writing the harness
  immediately showed that two of the three published rejection vectors were not
  enforced. The suite grew from six fixtures to forty-nine, covering an accept
  case for every message type in the control grammar, and the sanitization
  obligation gained its own vectors with hand-written expected output.
- `docs/spec/05-conformance.md` states the text-sanitization contract. It
  previously described the AML grammar without saying that anything had to be
  removed from text, so an implementation built from the contract alone would
  have rendered escape sequences and bidi overrides straight to the terminal.

- A trust prompt on `dustnet connect`. Reaching a site whose certificate cannot
  be verified — self-signed, expired, or issued for another name — with nothing
  pinned for that host and port now stops and asks, showing the site, why
  verification failed, and the certificate's full SHA-256 fingerprint so it can
  be compared against one the operator published. Accepting pins that exact
  certificate; declining connects to nothing. There is deliberately no third
  option: "continue without deciding" is the state that produces users with no
  idea what they are talking to. Answering the prompt changes the site's
  security context, so the navigation that provoked it is re-issued rather than
  resumed — the pinned origin is a different origin, and `prepare_navigation`
  refuses an owner carrying the one computed before the decision.
- Trust on first use for sites no certification authority vouches for.
  `dustnet connect <uri> --tofu` pins the SHA-256 of the site's certificate,
  keyed by the host and port that were typed, and every later connection must
  present the same one; a mismatch is a hard failure rather than a prompt,
  because a changed certificate is either a re-keyed server or an interception
  and a client cannot tell them apart. A pin is honoured afterwards without the
  flag, since it records a decision the user already made, while creating one
  always requires `--tofu` — so a store can never downgrade a connection that
  was not deliberately pinned. Pins live in `~/.config/dustnet/known_sites`
  (`DUSTNET_TRUST_STORE` overrides), are written owner-only, and the client
  refuses to read the file if other users can write to it: whoever can add a
  line chooses the certificate the client will accept. `dustnet trust list`
  and `dustnet trust forget <host[:port]>` manage it. `--tofu` remains as the
  non-interactive form of the prompt — it pins without asking, for scripts and
  for callers with no terminal, which instead receive an error naming the
  fingerprint and the reason and connect to nothing.

  Pinning skips host name verification and nothing else. The pin binds a
  certificate to the authority the user typed, which is what makes a
  self-signed certificate with no matching SAN usable; the handshake signature
  is verified against the pinned certificate exactly as usual, because a
  certificate is public and matching a fingerprint without checking the
  signature would authenticate everyone who had ever connected to the site
  rather than the holder of its private key.
- `dustnet connect --ca-file <PEM>` trusts additional certificate authorities
  alongside the built-in bundle, with host name verification unchanged.
- `TransportSecurity::PinnedTls`. Each level is part of origin identity, so a
  pinned session, a CA-verified session and an `--insecure` session to the same
  host and port are three partitions that never share state.

### Security

- A pinned certificate that changes now fails with its own message rather than
  a generic handshake error. It names both fingerprints and what to do about
  each, and it is never a prompt: by that point the only explanations are a
  re-keyed server and an interception, and asking again would train users to
  click through the one moment an interception is visible. Previously this
  reached the user as `failed to parse AML content`, because a first navigation
  that never activated was reported as a parse failure whatever went wrong.
- The status bar showed nothing at all for `--insecure`. The indicator was
  driven by a single `no_tls` boolean, so the one transport most able to
  mislead — encrypted, therefore looking protected, but unauthenticated —
  was the one that went unlabelled, while `docs/spec/07-security.md` claimed
  it was marked. `{security}` now names the transport in use (`no-tls`,
  `insecure`, `pinned`) and is empty only for CA-verified TLS. It is read from
  the live origin rather than the launch flags, so navigating between
  differently trusted sites changes what it claims.
- The text sanitizer now strips bidirectional controls and invisible formatting
  characters — U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069, U+200B,
  U+2060–U+2064, U+FFF9–U+FFFB and U+FEFF — as well as terminal control
  sequences. Escape stripping answers what the terminal will execute; it does
  not answer what the reader will believe. U+202E RIGHT-TO-LEFT OVERRIDE let a
  link label render as one URI while the link carried another, and the
  zero-width characters let two labels differing in bytes render identically,
  both inside the printable range the whitelist admitted. U+200C and U+200D are
  deliberately preserved: they compose emoji sequences and drive Arabic and
  Indic shaping, so removing them would corrupt conforming content. Hostnames
  were never exposed — the URI parser already rejects any non-ASCII host, which
  closes the IDN homograph vector.
- Control-message bodies now reject CR, NUL and every other ASCII control
  character, and must end in LF. `str::lines` silently strips a trailing CR, so
  `GET /\r\n` validated as a request for `/` while a peer splitting on LF alone
  read a path of `/\r`. Two implementations disagreeing about the same bytes,
  with the disagreement invisible to both, is the shape every request-smuggling
  bug has.

### Fixed

- The AML parser silently discarded everything after the closing `[/page]`. The
  grammar is `document = ws page ws`, so a second `[page]` root — or any other
  trailing content — was accepted as though it were not there, letting a server
  and a client disagree about what a document contained without either
  reporting a problem. Trailing content is now an `E001` diagnostic.
- The `ERROR` production in `docs/spec/05-conformance.md` described an inline
  message (`ERROR 404 Not Found`) that no implementation has ever emitted or
  accepted; the optional message has always been a `Message:` field on its own
  line, matching how every other control message carries optional data. The
  grammar was corrected to the wire format rather than the wire format to the
  grammar, so this is a documentation fix and not a protocol change.
- `ERROR` codes must be exactly three digits, and `REDIRECT` codes must be
  `301` or `302`. Both were parsed as `u16`, which admitted `ERROR 7` and
  `REDIRECT 303` — codes the conformance contract does not define and a client
  has no defined behaviour for.
- `REDIRECT` targets are validated as absolute `atp://` URIs at body
  validation. The client already resolved them with `AtpUri::parse` rather than
  `resolve`, so a relative or `http://` target was carried all the way to the
  point of use before failing.
- A live-region publish reported `subscribers=0` for the first read of any file
  while plainly serving a subscriber, because the read happened before the
  subscriber's receiver was registered. The receiver is now taken first, which
  also closes a window in which the registry could reap the entry during that
  read.

### Changed

- The default status bar is the framed, `{fill}`-separated layout the guide
  already documented, in grey on black rather than reverse video. Reverse
  takes whatever the terminal's foreground happens to be, so the bar was a
  black slab on light themes and a white one on dark; naming both colours
  makes it consistent, and `status-reverse = true` restores the old behaviour.
  `{security}` stays adjacent to `{uri}` in the default, because a format that
  drops it shows the same status bar for an `--insecure` link as for a
  CA-verified one.

- Live regions are read once per change and shared, rather than re-read by every
  connection on a 250ms timer. A file watched by a thousand viewers was being
  stat'd and read four thousand times a second to produce identical bytes;
  it is now read once, and subscribers share the result through a refcounted
  generation whose budget lease it carries, so its bytes are charged once and
  returned when the last subscriber lets go. Only the read is shared: each
  subscriber keeps its own delta baseline, so `Mode: delta` and `Mode: replace`
  subscribers of the same region are served correctly from one read, and a
  subscriber that misses a generation still resynchronises itself. Connections
  no longer hold a per-connection interval timer at all.
- The default server-wide connection ceiling is 2048, up from 64. The old number
  reflected per-connection subscription polling rather than any protocol limit.
  Holding a connection open costs memory; re-establishing one costs a TLS
  handshake, so keep-alive is the cheaper side of that trade and the ceiling is
  now set accordingly.
- Static path resolution no longer blocks a runtime worker. The site root is
  canonicalised once at bind time instead of on every request, and the
  `Path::exists` probe is gone because `canonicalize` already reports a missing
  path: three synchronous filesystem calls per GET and per SUBSCRIBE become one
  asynchronous call. A path that does not resolve is a 404, as before, rather
  than a containment error.
- Connections observe shutdown and leave voluntarily. Previously no connection
  task watched the shutdown signal, so one idle client was enough to burn the
  full drain deadline and abort every task; the deadline is now a backstop.
- Remote memory is bounded and admitted before allocation across cached
  resources, pending updates, history, scene geometry, and WASM. The mechanics
  are recorded in `verification/allocation-owners.md`.
- Function-local collection allocation in the compositor's scene-building and
  layout paths is counted and reserved before it is built, rather than grown
  from remote content. Enumerated and enforced by `tools/allocation-audit`.
- Unsafe Rust is forbidden in every production crate.
- `docs/spec/07-security.md` now covers only the supported production boundary; the
  authentication, email-verification, and plugin prose moved to
  `examples/unsupported-social`.
- Verification moved to a local `make ci` / `make ci-full` gate and the GitHub
  Actions workflow was removed — the account does not pay for Actions minutes,
  so every hosted run queued indefinitely and no gate was ever enforced. macOS
  is now the only verified platform; Linux is untested and best-effort.

---

Releasing is gated on a green `make ci` / `make ci-full` run, not on a review
period, an external sign-off, or a written assessment. All verification is our
own; see [SECURITY.md](SECURITY.md).

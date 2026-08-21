# Command-Line Reference

The reference implementation ships two binaries: `dustnet` for browsing and
authoring, and `dustnetd` for static serving.

## Commands (`dustnet`)

| Command | Description |
|---------|-------------|
| `dustnet render <file.aml>` | Render a local AML file to the terminal |
| `dustnet connect <atp://host>` | Connect to an ATP server and browse interactively |
| `dustnetd <directory> --cert CERT --key KEY` | Serve static AML files over ATP |
| `dustnet check <file.aml>` | Parse and validate an AML file, reporting errors |
| `dustnet dump-tokens <file>` | Display the token stream (debugging) |
| `dustnet dump-ast <file>` | Display the parsed AST (debugging) |
| `dustnet dump-cells <file>` | Display the rendered cell grid (debugging) |
| `dustnet trust list` | List pinned site certificates |
| `dustnet trust forget <host[:port]>` | Forget a pin, so the next `--tofu` connection remakes it |

### `dustnet connect` transport options

With no flags, certificates are verified against the built-in CA bundle, and a
site that cannot be verified raises a prompt showing its fingerprint. Accepting
pins it; declining connects to nothing. A site already pinned is reached by its
pin without any flag, and a pinned certificate that changes is refused outright
rather than re-asked.

The flags below are mutually exclusive, except that `--ca-file` accompanies
ordinary verification.

| Option | Effect |
|--------|--------|
| `--tofu` | Pin the first certificate seen *without asking*. The non-interactive form of the prompt, for scripts |
| `--ca-file <PEM>` | Trust additional authorities alongside the built-in bundle, host names still verified |
| `--insecure` | Verify nothing. Encrypted but unauthenticated, and nothing is carried forward |
| `--no-tls` | Plaintext. Loopback only |

Pins are stored in `~/.config/dustnet/known_sites`, one per line as
`host port sha256 first-seen`. The client refuses to read the file if other
users can write to it. `DUSTNET_TRUST_STORE` overrides the location.

A certificate that changes after being pinned is a hard failure. Remove the
line — with `dustnet trust forget` or an editor — only if the site was
genuinely re-keyed.

## Static server (`dustnetd`)

| Command | Description |
|---------|-------------|
| `dustnetd <directory> --cert FILE --key FILE` | Serve with TLS 1.3 |
| `dustnetd <directory> --cert FILE --key FILE --port 8080` | Use a custom port |
| `dustnetd <directory> --plaintext-loopback` | Plaintext loopback development mode |
| `dustnetd <directory> ... --log-format human` | Human-readable development logs (JSON is default) |

## Client Configuration

The client reads configuration from `~/.config/dustnet/client.conf`. Environment variables take priority over the config file; both override built-in defaults.

### Config File

These are the built-in defaults, written out so they can be changed. A file
that sets none of them behaves exactly as below.

```
# ~/.config/dustnet/client.conf

# Status bar format (connected mode)
status-format = {fill}[ {uri}{security} ]{fill}[ {scroll} {focus} ]{fill}[ {help} ]{fill}

# Status bar format (local file viewing)
status-local-format = {fill}[ dustnet ]{fill}[ {scroll} {focus} ]{fill}[ {help} ]{fill}

# Status bar colors
status-fg = #888888
status-bg = black
status-reverse = false
```

`status-reverse = true` uses reverse video instead, taking the terminal's own
foreground and background. That respects a user's palette, at the cost of
looking different on light and dark themes; clear `status-fg` and `status-bg`
to get it on its own, since reverse otherwise combines with them.

Keep `{security}` in any custom `status-format`. It is the only place the
interface distinguishes a pinned or unauthenticated connection from a
CA-verified one, so a format without it shows the same status bar for an
`--insecure` link as for a verified one.

### Status Bar and Command Line

The client reserves two rows at the bottom of the terminal. The **status bar** (second-to-last row) shows configurable information about the current page. The **command line** (last row) provides a vim-style `:` prompt for commands like `:open`, `:reload`, and `:quit`, and displays transient command and navigation messages.

Press backtick to open the client HUD. Its History and Errors tabs retain visit history and grouped runtime failures respectively. The first runtime failure opens the Errors tab automatically; later failures update the persistent status-bar count without interrupting input again. Error history lasts for the client session and can be cleared with `c` from the Errors tab.

### Status Bar Variables

| Variable | Description |
|----------|-------------|
| `{uri}` | Current page URI (connected mode only) |
| `{title}` | Page title from `[page title="..."]` |
| `{scroll}` | Scroll percentage (e.g. "42%"), empty if not scrollable |
| `{focus}` | Focus indicator (e.g. "[1/5]"), empty if no focusables |
| `{security}` | How the current connection was authenticated — `no-tls`, `insecure` or `pinned` — and empty for CA-verified TLS |
| `{help}` | Default keybinding hints |
| `{fill}` | Expands to `─` repeated to fill remaining terminal width |

### Environment Variables

| Variable | Overrides |
|----------|-----------|
| `DUSTNET_STATUS_FORMAT` | `status-format` |
| `DUSTNET_STATUS_LOCAL_FORMAT` | `status-local-format` |
| `DUSTNET_STATUS_FG` | `status-fg` |
| `DUSTNET_STATUS_BG` | `status-bg` |
| `DUSTNET_TRUST_STORE` | The pinned-certificate file path |

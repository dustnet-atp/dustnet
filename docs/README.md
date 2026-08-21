# Documentation map

Four kinds of document live here, and they are not interchangeable.

| Directory | Holds | Authority |
|---|---|---|
| [`spec/`](spec/) | ATP and AML themselves | Normative — binds *any* implementation |
| [`internals/`](internals/) | How this client is built | Descriptive — binds nothing |
| [`guides/`](guides/) | Using and operating Dustnet | Practical |
| [`adr/`](adr/) | Decisions and why they were taken | Historical |

Where `internals/` and `spec/` disagree, the specification wins and the
implementation is wrong.

## `spec/` — the specification

Start at the overview and read in order; each assumes the last.

| Document | Covers |
|---|---|
| [01-overview.md](spec/01-overview.md) | What Dustnet is: vision, terminology, architecture, pipeline, URI scheme |
| [02-protocol.md](spec/02-protocol.md) | ATP — the TLS 1.3 wire protocol, framing, and port 1985 |
| [03-markup.md](spec/03-markup.md) | AML — the declarative, non-Turing-complete content format |
| [04-rendering.md](spec/04-rendering.md) | The display model a conforming client must produce on screen |
| [05-conformance.md](spec/05-conformance.md) | The normative 0.2 conformance contract |
| [06-interactivity.md](spec/06-interactivity.md) | Panels, triggers, event bindings, forms, live regions, components |
| [07-security.md](spec/07-security.md) | The security model for the supported boundary |

None of these are executable. Nothing in the repository parses documentation:
the machine-checked claims live under [`../verification/`](../verification/) as
data, and the prose here explains them. Where a number appears in both, the
data file is the one the build reads.

## `internals/` — this implementation

This describes `crates/dustnet-client` and nothing beyond it. Paths inside it
are written relative to `crates/dustnet-client/src/compositor/`.

| Document | Covers |
|---|---|
| [compositor.md](internals/compositor.md) | How the reference client's display subsystem is structured |

## `guides/` — using it

| Document | Covers |
|---|---|
| [cli.md](guides/cli.md) | `dustnet` and `dustnetd` commands, client config file, status bar, environment variables |
| [production-support.md](guides/production-support.md) | What is supported, previewed, and out of scope; file-descriptor limits |

## `adr/` — decisions

| ADR | Decision |
|---|---|
| [0001](adr/0001-production-boundary.md) | Production boundary and staged release |
| [0002](adr/0002-origin-resources-viewer.md) | Origin ownership, resource rejection, viewer reduction |
| [0003](adr/0003-workspace-boundaries.md) | Workspace package boundaries during 0.x |

## Not here

Two things deliberately live elsewhere.

The threat model, allocation inventory, conformance limits, protocol state
table, fuzz campaign and install manifest are in
[`../verification/`](../verification/). Those files are inputs to `make ci`
rather than documentation, and are read by `tools/allocation-audit` as
machine-checkable claims about the code.

Defects found and not yet fixed are registered in
[`../verification/BUGS.md`](../verification/BUGS.md), each with what it costs to
leave. That one is prose and nothing reads it; what is merely *absent* rather
than broken is in [production-support.md](guides/production-support.md).

The unsupported dynamic-server prototype — accounts, email verification,
boards, chat, links, and the plugin and template-marker mechanism — is
documented with the code it describes, in
[`../examples/unsupported-social/README.md`](../examples/unsupported-social/README.md).

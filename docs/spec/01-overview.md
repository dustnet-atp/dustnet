# Dustnet — Project Overview

## Vision

**Dustnet** is a terminal-native network — a decentralized network of sites rendered in rich ANSI/ASCII art, linked together with animated transitions, all served over an encrypted protocol.

The premise: what if the web had been built for terminals?

## Core Principles

1. **Declarative content, sandboxed code.** Site *content* is declarative markup — the client renders, the server describes, and markup can neither branch nor reach a system resource. The one executable element, WASM animation effects, runs only inside a draw-only, fuel-metered sandbox that can do nothing but write cells to a bounded canvas (see [07-security.md](07-security.md)). The foundational guarantee is not "no code runs" but "the only code that runs cannot escape the renderer."
2. **Terminal-native.** Content is designed for character cells, not pixels. ANSI color, Unicode box-drawing, block elements, and braille characters are first-class citizens.
3. **Decentralized.** No central server. Anyone can run a site.
4. **Beautiful transitions.** Moving between sites is an experience, not a page load. Sites declare transitions; clients animate them.
5. **Safe by design.** No content scripting language, no eval, no shell access. The attack surface is a markup renderer plus a draw-only WASM sandbox — not a browser engine.

## Terminology

| Term | Definition |
|------|------------|
| **Dustnet** | The decentralized network and its ecosystem |
| **ATP** | ANSI Terminal Protocol — the wire protocol (TLS 1.3 over TCP, port 1985) |
| **AML** | ANSI Markup Language — the declarative content format |
| **Site** | A server running ATP, analogous to a website |
| **Page** | A single unit of content served by a site, analogous to a web page |
| **Client** | The terminal application that connects to sites and renders content |
| **Panel** | A page region that can switch between declared visual states in response to user actions |
| **Component** | A reusable AML template defined with `[def]`, expanded at parse time |
| **Transition** | An animated visual effect when navigating between pages or sites |
| **Session** | A path-scoped authentication token stored by the client for a specific site, never shared cross-site |
| **Plugin** | Unsupported historical example machinery; not part of production `dustnetd` |
| **Compositor** | The layer management system that composites multiple cell buffers into a single output |
| **Layer** | A named cell buffer at a specific z-index in the compositor; transparent cells let lower layers show through |

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   Dustnet Client                        │
│  ┌──────────┐ ┌───────────┐ ┌──────────┐ ┌───────────┐ │
│  │ Renderer │ │Compositor │ │Transition│ │ Navigation│ │
│  │  Engine  │ │ (layers)  │ │  Engine  │ │  & History│ │
│  └──────────┘ └───────────┘ └──────────┘ └───────────┘ │
│                        │ TLS 1.3                        │
└────────────────────────┼────────────────────────────────┘
                         │
          ┌──────────────┼──────────────┐
          │              │              │
   ┌──────▼──────┐ ┌────▼─────┐ ┌──────▼──────┐
   │  Site A     │ │  Site B  │ │  Site C     │
   │ (neon.city) │ │(dark.zone)│ │(art.gallery)│
   │             │ │          │ │             │
   │ ATP Server  │ │ATP Server│ │ ATP Server  │
   └─────────────┘ └──────────┘ └─────────────┘
```

The repository is a virtual Cargo workspace with five production packages:

| Package | Responsibility |
|---------|----------------|
| `dustnet-core` | ATP/AML values, codecs, parsing, origin rules, and protocol state machines |
| `dustnet-client` | Client transport, sessions, viewer state, compositor, and WASM host |
| `dustnet-server` | Plugin-free `StaticServer` and `StaticServerConfig` |
| `dustnet` (`crates/dustnet`) | Client, local rendering, and authoring CLI |
| `dustnetd` | Static-server CLI |

There is no root facade or compatibility crate. The production server API has
no `AtpServer`, authentication, custom-handler, or plugin surface. Historical
social/plugin code is confined to `examples/unsupported-social`, which is
excluded from the workspace.

## Processing Pipeline

Content flows through four distinct stages:

1. **Scanner** — raw UTF-8 bytes become tokens (tags, attributes, text)
2. **Parser** — tokens become a typed abstract syntax tree (AST), with component expansion and validation
3. **Layout Engine** — the AST and terminal dimensions produce a 2D grid of styled character cells
4. **Renderer** — the cell grid is composited through layers and emitted as ANSI escape sequences to the terminal

Each stage has clear boundaries. The parser rejects malformed documents. The layout engine enforces spatial limits. The renderer generates only its own escape sequences — nothing from the server reaches the terminal directly.

## URI Scheme

ATP uses the `atp://` URI scheme:

```
atp://neon.city/                    — site root
atp://neon.city/gallery/pixel-art   — specific page
atp://neon.city/boards?page=2       — paginated content
atp://neon.city/chat/general#live   — live-updating region
```

Only `atp://` URIs are navigable. No `file://`, `http://`, `ssh://`, or other scheme handling exists in the client.

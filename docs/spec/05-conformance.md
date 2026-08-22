# ATP/AML 0.2 Conformance Contract

This document is normative for the `0.2` preview. Preview status does not make
the contract stable: incompatible corrections may still occur before ATP/AML
1.0 is frozen.

## ATP framing

All integers are unsigned. `length` includes the six-byte header.

```text
frame       = length:u32be type:u8 flags:u8 body:(length - 6 bytes)
length      = 6..16777216
client-type = 01 / 02 / 03 / 04 / 05 / 06 / 0f
server-type = 81 / 82 / 83 / 84 / 85 / 86 / 87 / 8f
```

Unknown types, wrong-direction types, lengths below six, undefined flag bits,
and semantic body lengths above the per-message limit are fatal protocol
errors. Every non-RESOURCE body is strict UTF-8. Control lines end in LF;
CR, NUL, other controls, duplicate singleton fields, malformed fields, and
unknown fields are rejected.

## ATP control grammar

The notation is ABNF-like. `path`, `value`, and AML text must also pass the
implementation validation described in the linked protocol and markup docs.

```text
LF          = %x0A
SP          = %x20
DIGIT       = %x30-39
lower       = %x61-7A
capability  = 1*(lower / "-")
caps        = capability *("," capability)
field       = field-name ": " value LF

HELLO       = "HELLO/0.2" LF
              ["Terminal-Size: " value LF]
              ["Color-Support: " value LF]
              ["Client: " value LF]
              ["Capabilities: " caps LF]

WELCOME     = "WELCOME/0.2" LF
              ["Server: " value LF]
              ["Site-Name: " value LF]
              ["Capabilities: " caps LF]

GET         = "GET " path LF
              ["Query: " value LF]
              ["Referrer: " value LF]
              ["Session: " value LF]

INPUT       = "INPUT " path LF
              ["Form: " value LF]
              ["Session: " value LF]

SUBSCRIBE   = "SUBSCRIBE " path LF
              ["Region: " value LF]
              ["Mode: " ("replace" / "delta") LF]
              ["Session: " value LF]

UNSUBSCRIBE = <empty body>
PING        = <empty body>
PONG        = <empty body>
BYE         = <empty body>
SERVER-BYE  = <empty body>

REDIRECT    = "REDIRECT " ("301" / "302") SP atp-uri LF
ERROR       = "ERROR " 3DIGIT LF
              ["Message: " value LF]
UPDATE      = "UPDATE " value LF LF aml-fragment

PAGE        = aml-document
            / page-field *(page-field) LF aml-document
page-field  = "Path: " path-query LF
            / "Set-Session: " value LF
            / "Clear-Session: " value LF
path-query  = path ["?" value]
RESOURCE    = *OCTET
```

HELLO capabilities may contain future names. WELCOME may select only names
offered by HELLO. ATP 0.2 behavior is enabled only for the intersection of
`live-updates`, `sessions`, `wasm-effects`, and `page-path`.
UPDATE/SUBSCRIBE/UNSUBSCRIBE, session-bearing PAGE/requests, a PAGE naming its
own path, and RESOURCE/WASM behavior without the matching negotiated capability
are fatal errors. A PAGE setting several flags must satisfy the capability each
one requires, not merely the first. PING and PONG are core 0.2 frames and
are not capability-gated, but like every other application frame they are fatal
before HELLO/WELCOME negotiation completes.

## Exhaustive connection state table

All transitions not listed are invalid and close/poison the connection. A
request means GET or INPUT. UPDATE is the only frame allowed to interleave
while a request response is pending. PING and PONG are liveness frames: they
carry no body, never change the connection state, and are legal in both Ready
and ResponsePending so that a slow response cannot starve the keepalive.

The transitions themselves are data rather than prose:
[`verification/protocol-state-table.json`](../../verification/protocol-state-table.json)
is the authoritative list, and `dustnet-core`'s
`documented_state_table_matches_implementation` test asserts the implementation
matches it. Nothing parses this document, so the table cannot drift from the
machine by being edited here.

## AML lexical grammar

```text
document     = ws page ws
page         = "[page" attributes "]" node* "[/page]"
node         = text / element / self-closing
element      = "[" name attributes "]" node* "[/" name "]"
self-closing = "[" name attributes ws? "/]"
attributes   = *(1*ws attribute)
attribute    = name ["=" (quoted / bare)]
quoted       = DQUOTE *(escaped / nonquote) DQUOTE
bare         = 1*(visible except ws, "[", "]", DQUOTE)
escaped      = "\\" ("\\" / DQUOTE / "[" / "]" / "n" / "t")
name         = ALPHA *(ALPHA / DIGIT / "-")
text         = *(escaped / any Unicode scalar except unescaped "[")
ws           = SP / HTAB / CR / LF
```

Tag and attribute names are ASCII-case-insensitive. The complete allowed
element/attribute vocabulary and nesting rules are the tables in
[03-markup.md](03-markup.md), while panel/event/form/live semantics are in
[06-interactivity.md](06-interactivity.md). Unknown elements and attributes
produce diagnostics and are never executable extension points. AML has one
`page` root, a maximum 32 nesting depth, bounded token/component expansion,
at most 1,024 animation regions, at most 256 authored frames, and at most 16
WASM animation guests.

## Text sanitization

A conforming client MUST remove the following from text before rendering it,
rather than rejecting the document that carries them. Both classes are
server-chosen bytes that a terminal would otherwise present as trustworthy.

1. **Terminal controls.** C0 controls other than HTAB and LF, DEL, ESC and the
   sequence it introduces, and the C1 block U+0080–U+009F including the
   sequences introduced by DCS, CSI, OSC, PM and APC. CR is normalized to LF.
2. **Deceptive formatting.** Bidirectional marks (U+061C, U+200E, U+200F),
   explicit bidi embedding and override (U+202A–U+202E), bidi isolates
   (U+2066–U+2069), zero-width space and the invisible operators (U+200B,
   U+2060–U+2064), interlinear annotation (U+FFF9–U+FFFB), and U+FEFF.

U+200C and U+200D MUST be preserved: they compose emoji sequences and drive
Arabic and Indic shaping, so removing them corrupts conforming content.

Sanitization applies to text content and to any server-supplied string the
client renders in its own chrome, including diagnostics quoting remote input.
`tests/conformance/` carries the vectors.

## Normative implementation limits

The values are data rather than prose:
[`verification/conformance-limits.json`](../../verification/conformance-limits.json)
is the authoritative table, and `dustnet-client`'s
`documented_limits_match_implementation` test asserts each entry equals the
constant that enforces it. A limit therefore cannot be changed in code while
the contract still claims the old value.


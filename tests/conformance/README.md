# ATP/AML 0.2 Conformance Fixtures

These vectors are what an independent implementation can check itself against.
`docs/spec/05-conformance.md` is the contract in prose; these are the same
contract as files.

| Directory | Obligation |
|---|---|
| `valid/` | MUST be accepted |
| `invalid/` | MUST be rejected |
| `sanitize/` | MUST be transformed into the sibling `.expected` file |

`.atp` files are wire-message **bodies**: no frame header, LF line endings,
UTF-8. `.aml` files are whole documents.

## `vectors.json`

A body is just bytes. Nothing in a filename says which message it is, which
flags accompanied it, or what a peer had already agreed to — so
[`vectors.json`](vectors.json) records that, and only that. The expectation is
*not* recorded there: the directory decides it, so the two cannot drift apart.

Some obligations cannot be judged from a body at all, and the manifest carries
what is needed to judge them:

- `invalid/unoffered-welcome.atp` is a well-formed WELCOME. "May select only
  names HELLO offered" is a claim about a connection, so the entry carries the
  offer it is judged against.
- `invalid/*-unsupported-version.atp` are well-formed too. A peer has to parse
  a version it does not speak in order to say so, which is why the announced
  version is compared to `PROTOCOL_VERSION` rather than rejected by the
  grammar.

A conforming implementation is expected to apply the same layers.

## Running them

`dustnet-core`'s `conformance_vectors` integration test runs every vector on
every `cargo test`. It also fails if a `.atp` file on disk has no manifest
entry, or an entry names a file that is gone — a fixture nobody runs proves
nothing, and running the suite is the only thing that keeps these vectors
honest. Before that test existed the `.atp` files here were read by nothing,
and two of the three published rejection vectors were not in fact enforced.

## What is deliberately not here

Unclosed and mismatched tags are **warnings** (W003, W004), not rejections:
AML recovers rather than discarding the document, so
`valid/aml-unclosed-element-recovers.aml` is published as accepted. Anything
whose diagnostics are warning-level belongs under `valid/`.

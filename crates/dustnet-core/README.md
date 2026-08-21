# dustnet-core

ATP/AML protocol, parser, scanner, URI, origin and session primitives for
[Dustnet](https://github.com/roobert/dustnet), a terminal hypertext protocol.

No client or server dependency: this crate is the shared substrate both sides
build on. Every panicking operation on a remote-input path is denied at the
crate root, so malformed input fails as an error rather than an abort.

Licensed under MIT.

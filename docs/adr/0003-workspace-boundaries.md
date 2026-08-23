# ADR 0003: Workspace Package Boundaries During 0.x

Status: accepted

The repository is a virtual workspace with five production packages:
`dustnet-core`, `dustnet-client`, `dustnet-server`, the `dustnet` client CLI,
and the `dustnetd` static-server CLI. There is no root compatibility package or
facade crate. Production source is physically owned by its package.

`dustnet-core` owns ATP/AML values, codecs, messages, and state machines, and
does not depend on either transport implementation. `dustnet-client` and
`dustnet-server` own their respective transports and both depend on core. The
server package exposes `StaticServer` and `StaticServerConfig`; it has no
`AtpServer`, custom-handler, authentication, or plugin API.

Social, authentication, and plugin experiments were quarantined in
`examples/unsupported-social`, outside the default workspace. That prototype was
deleted on 2026-08-23 once a real application built on the hooks superseded it;
see the third amendment to
[0001-production-boundary.md](0001-production-boundary.md). No such experiment
remains in the tree.

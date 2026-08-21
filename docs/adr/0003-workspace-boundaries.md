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

Historical social, authentication, and plugin experiments are quarantined in
`examples/unsupported-social`, outside the default workspace. They are examples
only and are not supported production behavior.

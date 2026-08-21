# dustnet-client

Terminal client, compositor, navigation and AML/WASM runtime for
[Dustnet](https://github.com/roobert/dustnet).

Remote content reaches every parser, layout and rendering path here, so the
crate is built around bounded resource use: allocations on remote-influenced
paths are admitted through a governor, and panicking operations are denied at
the crate root.

Licensed under MIT OR Apache-2.0.

# Dustnet

Dustnet is an experimental terminal-native network. Sites serve declarative
ANSI Markup Language (AML) over the TLS-based ANSI Terminal Protocol (ATP), and
the reference client lays pages out as terminal cells. Procedural effects run
inside a fuel- and memory-limited, draw-only WASM interpreter.

The premise: what if the web had been built for terminals?

## Connecting to a site

There is a live site at `dustnet.io`. Any of the three routes below ends at the
same command, so pick whichever suits how much you want installed.

### From crates.io

`0.2.0-alpha.1` is a pre-release, so its version has to be named. Cargo does
not select pre-release versions for a bare requirement, which is why a plain
`cargo install dustnet` reports that it cannot find the crate.

```console
cargo install dustnet --version 0.2.0-alpha.1 --locked
dustnet connect atp://dustnet.io
```

### From source

```console
git clone https://github.com/roobert/dustnet
cd dustnet
cargo run --release --locked -p dustnet -- connect atp://dustnet.io
```

### With Docker

Build once. The result carries the client and nothing else — the Rust
toolchain stays behind in the discarded build stage.

```console
docker build -t dustnet - <<'EOF'
FROM rust:1.94-slim AS b
RUN cargo install dustnet --version 0.2.0-alpha.1 --locked --root /out
FROM debian:trixie-slim
COPY --from=b /out/bin/dustnet /usr/local/bin/
ENTRYPOINT ["dustnet"]
EOF

mkdir -p ~/.config/dustnet
```

Then, for each run:

```console
docker run --rm -it \
  -e TERM=xterm-256color \
  --user "$(id -u):$(id -g)" \
  --read-only --tmpfs /tmp \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --pids-limit 128 --memory 512m \
  -e HOME=/state \
  -v "$HOME/.config/dustnet:/state/.config/dustnet" \
  dustnet connect atp://dustnet.io
```

The container is disposable but `~/.config/dustnet` is not, and that split is
deliberate. Pinning earns its value by remembering: a client that forgets every
time treats every visit as a first visit, and a fingerprint prompt that appears
on every run is one nobody reads. The flags above drop every capability, keep
the root filesystem read-only, and run as you rather than as root, because the
client parses wire data and runs remote WASM from hosts it does not control.

## About AML and ATP

AML describes layout, text, colour, interactive elements and animation, but
never logic — it is not Turing-complete and has no scripting capability. ATP
runs over TLS 1.3 on TCP, port 1985.

## Status

Dustnet is pre-production. **ATP/AML 1.0 is not frozen**, so neither protocol
compatibility nor the security posture is a stable-release guarantee, and all
verification is our own — there is no external review or sign-off.

[docs/guides/production-support.md](docs/guides/production-support.md) is the
authority on what is supported, previewed and out of scope, and on which
platforms carry a verified gate.

## Other commands

```console
dustnet render page.aml
dustnet check page.aml
dustnet connect atp://example.com/
dustnetd ./site --cert cert.pem --key key.pem
```

A site with no certification authority behind it — the common case when anyone
can run one — prompts on first visit with its certificate fingerprint. Accepting
pins it, in the way SSH pins a host key: later visits need no flag, and a
certificate that changes is refused rather than re-asked.

```console
dustnet connect atp://example.com/
dustnet trust list
```

Plaintext mode is deliberately loopback-only and intended for development:

```console
dustnetd ./site --plaintext-loopback
dustnet connect atp://127.0.0.1:1985/ --no-tls
```

## Documentation

Start with the [project overview](docs/spec/01-overview.md), then read the
[protocol](docs/spec/02-protocol.md), [markup](docs/spec/03-markup.md),
[rendering model](docs/spec/04-rendering.md), and
[security model](docs/spec/07-security.md).

[docs/README.md](docs/README.md) maps the full set and explains which documents
are normative and which merely describe this implementation. The full command,
configuration and status-bar reference is in
[docs/guides/cli.md](docs/guides/cli.md).

## Development

Rust is the only required toolchain for the core client and server:

```console
cargo build --all-features
cargo test --all-features
```

The Cargo workspace has separately compilable production surfaces:
`dustnet-core` (ATP/AML and origin primitives), `dustnet-client` (terminal
client/runtime), `dustnet-server` (static server), `dustnet` (client/authoring
CLI), and `dustnetd` (server CLI). The repository root is a virtual workspace.

All verification runs locally — there is no hosted CI:

```console
make test      # fast inner loop
make ci        # the full gate; run before every commit
make ci-full   # ci plus Miri, ASan, and a fuzz smoke run
```

`make test` also checks that the separately packaged fuzz targets still
compile. See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a change, and
[CHANGELOG.md](CHANGELOG.md) for what has changed between versions.

## Licence

Licensed under either MIT or Apache-2.0, at your option. See
[SUPPORT.md](SUPPORT.md), [SECURITY.md](SECURITY.md), and
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

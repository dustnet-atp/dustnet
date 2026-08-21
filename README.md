# Dustnet

An experimental terminal-native network. Sites serve declarative ANSI Markup
Language (AML) over the TLS-based ANSI Terminal Protocol (ATP), and the
reference client lays pages out as terminal cells.

The premise: what if the web had been built for terminals?

## Connecting to a site

There is a live site at `dustnet.io`. Every route below ends at the same
command, so the choice is only about how much you want installed.

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

### With Docker, without building anything

One command, no image and no Dockerfile. The first run compiles and takes a
couple of minutes; every run after it starts in well under a second, because
the built binary stays in a volume and the guard skips straight past
`cargo install`.

```console
docker run --rm -it \
  -e TERM=xterm-256color \
  -v dustnet-bin:/opt/dustnet \
  -v dustnet-cargo:/usr/local/cargo/registry \
  -v "$HOME/.config/dustnet:/root/.config/dustnet" \
  rust:1.94-slim \
  sh -c '[ -x /opt/dustnet/bin/dustnet ] || cargo install dustnet --version 0.2.0-alpha.1 --locked --root /opt/dustnet; exec /opt/dustnet/bin/dustnet connect atp://dustnet.io'
```

This is the least contained of the four. It runs as root inside the container
and keeps a full Rust toolchain in the image it runs from, so it suits a first
look rather than regular use — the route above costs one build and gives up
neither. On Linux the trust-store file it writes will belong to root on the
host. `docker volume rm dustnet-bin dustnet-cargo` discards everything it
cached.

## Everything else

[docs/README.md](docs/README.md) maps the documentation and says which parts are
normative. [docs/guides/cli.md](docs/guides/cli.md) is the full command
reference, and
[docs/guides/production-support.md](docs/guides/production-support.md) states
what is supported and what is not. [CONTRIBUTING.md](CONTRIBUTING.md) covers
building and testing.

Licensed under either MIT or Apache-2.0, at your option.

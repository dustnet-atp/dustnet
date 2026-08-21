# Dustnet

An experimental terminal-native network. Sites serve declarative ANSI Markup
Language (AML) over the TLS-based ANSI Terminal Protocol (ATP), and the
reference client lays pages out as terminal cells.

The premise: what if the web had been built for terminals?

## Connecting to a site

`dustnet.io` is live. Any route below gets you there.

### From crates.io

```console
cargo install dustnet --version 0.2.0-alpha.1 --locked
dustnet connect atp://dustnet.io
```

The version is required — Cargo skips pre-releases for a bare requirement.

### From source

```console
git clone https://github.com/roobert/dustnet
cd dustnet
cargo run --release --locked -p dustnet -- connect atp://dustnet.io
```

### Docker

Build once:

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

Then per run:

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

`~/.config/dustnet` is mounted so certificate pins survive between runs.

### Docker, without building an image

First run compiles; later runs start in under a second.

```console
docker run --rm -it \
  -e TERM=xterm-256color \
  -v dustnet-bin:/opt/dustnet \
  -v dustnet-cargo:/usr/local/cargo/registry \
  -v "$HOME/.config/dustnet:/root/.config/dustnet" \
  rust:1.94-slim \
  sh -c '[ -x /opt/dustnet/bin/dustnet ] || cargo install dustnet --version 0.2.0-alpha.1 --locked --root /opt/dustnet; exec /opt/dustnet/bin/dustnet connect atp://dustnet.io'
```

Runs as root, from an image carrying a Rust toolchain. Reset with
`docker volume rm dustnet-bin dustnet-cargo`.

## Everything else

[docs/README.md](docs/README.md) maps the documentation.
[docs/guides/cli.md](docs/guides/cli.md) is the command reference.
[docs/guides/production-support.md](docs/guides/production-support.md) states
what is supported. [CONTRIBUTING.md](CONTRIBUTING.md) covers building and
testing.

Licensed under either MIT or Apache-2.0, at your option.

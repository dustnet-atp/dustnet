# Dustnet

An experimental terminal-native network. Sites serve declarative ANSI Markup
Language (AML) over the TLS-based ANSI Terminal Protocol (ATP), and the
reference client lays pages out as terminal cells.

The premise: what if the web had been built for terminals?

## Connecting to a site

`dustnet.io` is live. Any route below gets you there.

### From crates.io

```console
cargo install dustnet --version 0.2.0-alpha.3 --locked
dustnet connect atp://dustnet.io
```

The version is required — Cargo skips pre-releases for a bare requirement.

### From source

```console
git clone https://github.com/dustnet-atp/dustnet
cd dustnet
cargo run --release --locked -p dustnet -- connect atp://dustnet.io
```

### Docker

```console
docker build -t dustnet .
docker run --rm -it -v "$HOME/.config/dustnet:/root/.config/dustnet" \
  dustnet connect atp://dustnet.io
```

The mount keeps certificate pins between runs.

## Everything else

[docs/README.md](docs/README.md) maps the documentation.
[docs/guides/cli.md](docs/guides/cli.md) is the command reference.
[docs/guides/production-support.md](docs/guides/production-support.md) states
what is supported. [CONTRIBUTING.md](CONTRIBUTING.md) covers building and
testing.

Licensed under [MIT](LICENSE-MIT).

```
██████╗ ██╗   ██╗███████╗████████╗███╗   ██╗███████╗████████╗
██╔══██╗██║   ██║██╔════╝╚══██╔══╝████╗  ██║██╔════╝╚══██╔══╝
██║  ██║██║   ██║███████╗   ██║   ██╔██╗ ██║█████╗     ██║
██║  ██║██║   ██║╚════██║   ██║   ██║╚██╗██║██╔══╝     ██║
██████╔╝╚██████╔╝███████║   ██║   ██║ ╚████║███████╗   ██║
╚═════╝  ╚═════╝ ╚══════╝   ╚═╝   ╚═╝  ╚═══╝╚══════╝   ╚═╝
          ANSI TERMINAL PROTOCOL NETWORK * Est. 2026
```

**[dustnet.io](https://dustnet.io) | `atp://dustnet.io`**

DUSTNET is home to a network of sites built on the ANSI Terminal Protocol.
Build whatever you want. Make it look like nothing else.

This repository is the reference implementation: the client, the server, and
the specification for ATP and AML.

## What DUSTNET is

DUSTNET is an art project. A throwback to the days of the BBS, 2400 baud,
blinking lights, the modem screaming into the darkness, ANSI art unrolling one
line at a time.

It is an experiment. Is it secure? Probably not. Run your client in a
container, if you run a site, isolate it. Don't submit anything you want to
stay secret. Hold on to the seat of your pants and throw yourself head first
through your monitor into this network at the edge of the internet.

Stand up a site and add it to the HUB. Argue on the NEWS board. Leave
something behind for the next spec of DUST.

Sites serve declarative ANSI Markup Language (AML) over the TLS-based ANSI
Terminal Protocol (ATP), and the client lays pages out as terminal cells. A
site sends meaning, not a remote application: your client decides how that
meaning becomes colour, layout, focus, motion, and terminal output.

## Connecting

**Use Docker.** This is an experiment that renders untrusted content from
strangers straight into your terminal — a container is the boundary you want,
and it keeps a Rust toolchain off your machine.

```console
docker run --rm -it -v "$HOME/.config/dustnet:/root/.config/dustnet" \
  ghcr.io/dustnet-atp/dustnet connect atp://dustnet.io
```

The mount keeps certificate pins between runs. Drop it and the client treats
every site as new on every launch.

<details>
<summary>Without Docker</summary>

From crates.io:

```console
cargo install dustnet --locked
dustnet connect atp://dustnet.io
```

From a checkout:

```console
cargo run --release --locked -p dustnet -- connect atp://dustnet.io
```

Or build the image from source instead of pulling it:

```console
make docker-run
```

</details>

## The network

Once you are connected, these are the places to go.

| Site | Address | What it is |
|---|---|---|
| DUSTNET | `atp://dustnet.io` | The index, and a gallery of what the medium can do |
| DUSTHUB | `atp://hub.dustnet.io:1987` | Discover and submit DUSTNET sites |
| DUSTNEWS | `atp://news.dustnet.io:1986` | News, links and community discussion |

`atp://dustnet.io/about` explains how DUSTNET works from the inside.

## Everything else

[docs/README.md](docs/README.md) maps the documentation.
[docs/guides/cli.md](docs/guides/cli.md) is the command reference.
[docs/guides/production-support.md](docs/guides/production-support.md) states
what is supported. [CONTRIBUTING.md](CONTRIBUTING.md) covers building and
testing.

The prose above mirrors the DUSTNET index page, which is authored in the site
repository and is the copy of record.

Licensed under [MIT](LICENSE-MIT).

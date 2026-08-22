FROM rust:1.94-slim AS build
WORKDIR /src
COPY . .
RUN cargo install --locked --path crates/dustnet --root /out

FROM debian:trixie-slim

# GHCR links a package to a repository by org.opencontainers.image.source, and
# only a linked package appears in the repository's sidebar or inherits its
# README. Without this the image publishes fine and is simply orphaned: present
# in the registry, invisible from the repository anyone arrives at first.
LABEL org.opencontainers.image.source="https://github.com/dustnet-atp/dustnet"
LABEL org.opencontainers.image.description="The Dustnet reference client: browses ANSI Markup Language over the ANSI Terminal Protocol."
LABEL org.opencontainers.image.licenses="MIT"

COPY --from=build /out/bin/dustnet /usr/local/bin/dustnet
ENTRYPOINT ["dustnet"]

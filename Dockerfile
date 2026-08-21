FROM rust:1.94-slim AS build
WORKDIR /src
COPY . .
RUN cargo install --locked --path crates/dustnet --root /out

FROM debian:trixie-slim
COPY --from=build /out/bin/dustnet /usr/local/bin/dustnet
ENTRYPOINT ["dustnet"]

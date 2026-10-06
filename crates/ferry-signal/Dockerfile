# ferry-signal container image. Build from the repository root (the crate is
# part of the Cargo workspace):
#
#   docker build -f crates/ferry-signal/Dockerfile -t ferry-signal .
#
# Dockerfile.dockerignore (next to this file) keeps build outputs and
# node_modules out of the context.

FROM rust:1.97-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p ferry-signal

# glibc + libgcc only; no shell, no package manager, runs as uid 65532.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/ferry-signal /usr/local/bin/ferry-signal
# Inside a container the server must listen on all interfaces; publish the
# port on the host's loopback (-p 127.0.0.1:3000:3000) behind a TLS proxy.
ENV FERRY_SIGNAL_ADDR=0.0.0.0:3000
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s \
    CMD ["/usr/local/bin/ferry-signal", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/ferry-signal"]

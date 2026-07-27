# ---------------------------------------------------------------------------
# Stage 1: "builder" — a fat image with the whole Rust toolchain.
#
# We use a MULTI-STAGE build: this stage compiles the binary, then we throw
# the entire stage away and copy only the finished binary into a tiny runtime
# image. The Rust toolchain (~2 GB) never ships to production.
#
# rust:1-bookworm = latest stable Rust on Debian 12 ("bookworm"). We pin the
# Debian release so the glibc version matches the runtime stage below.
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /app

# Copy the workspace in. .dockerignore keeps target/, .git/, android/ out,
# so this is just source code and Cargo manifests.
COPY . .

# Build only the server crate (-p server), in release mode.
#
# The --mount=type=cache lines are BuildKit "cache mounts": persistent
# directories that survive between builds. Without them, every `docker build`
# starts from a cold target/ dir and recompiles ALL dependencies (~minutes).
# With them, a rebuild after a small source edit only recompiles your crates.
#
# Because the cache mount "owns" target/ (it vanishes after the RUN step),
# we copy the finished binary out to a normal path in the same command.
RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release -p server && \
    cp target/release/server /usr/local/bin/simulife-server

# ---------------------------------------------------------------------------
# Stage 2: the runtime image — starts from scratch, tiny (~75 MB vs ~2 GB).
#
# debian:bookworm-slim matches the builder's glibc, so the binary just works.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

# Run as a non-root user: if the process is ever compromised, the attacker
# isn't root inside the container. Cheap insurance, standard practice.
RUN useradd --create-home --user-group simulife
USER simulife

# /data is where world snapshots go; mount a volume here to persist them.
WORKDIR /data

# Grab ONLY the compiled binary from the builder stage.
COPY --from=builder /usr/local/bin/simulife-server /usr/local/bin/simulife-server

# Documentation for humans + tooling: the QUIC listener is UDP port 4433.
# (EXPOSE doesn't actually open anything — publishing happens at `docker run`.)
EXPOSE 4433/udp

# ENTRYPOINT = the fixed command. --listen 0.0.0.0:4433 is critical:
# the default 127.0.0.1 binds the container's OWN loopback, which nothing
# outside the container can reach.
#
# Anything you pass after `docker run <image>` gets APPENDED, so extra
# flags still work:  docker run simulife-server --seed 42 --start-running
ENTRYPOINT ["/usr/local/bin/simulife-server", "--listen", "0.0.0.0:4433"]

# The BoaVoice server, as a container.
#
# Only the server. The client is a desktop app with a GPU surface, audio devices and a screen to
# capture; there is nothing sensible to run in a container, and building it here would drag ALSA,
# ffmpeg and an H.264 codec into an image whose whole job is to forward packets.
#
# Two stages, and the first one is split in two on purpose: the dependencies are built against stub
# sources first, so that editing the server's code re-uses a cached layer of ~200 compiled crates
# instead of rebuilding them. That turns an iteration from two minutes into fifteen seconds, which
# matters when the point of this image is to be rebuilt often.

# --------------------------------------------------------------------------- #
# `rust:1-slim` rather than a pinned patch version: this image is meant to be rebuilt often, the
# workspace's `rust-version` is the real floor, and a pinned tag here would mean editing the
# Dockerfile every time the floor moves. Reproducibility comes from Cargo.lock, which is committed.
FROM rust:1-slim AS build

# `build-essential` for the C compiler rusqlite's bundled SQLite needs. Nothing else: the server has
# no audio, no video and no TLS of its own — a reverse proxy in front of it does that.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# --- dependencies, against stubs ------------------------------------------- #
# Every manifest, so the workspace resolves, plus a placeholder for each member's targets. The client
# is never built — `cargo build -p boa-server` does not touch it — but its manifest has to exist for
# the workspace to be valid.
COPY Cargo.toml Cargo.lock ./
COPY crates/boa-proto/Cargo.toml crates/boa-proto/
COPY crates/boa-server/Cargo.toml crates/boa-server/
COPY crates/boa-client/Cargo.toml crates/boa-client/
# The stub artefacts are deleted at the end of this step — binary, fingerprints and rlibs — because
# otherwise cargo sees an up-to-date build of the real crates in the next step and ships a server that
# does nothing at all. Their *dependencies* stay cached, which is the whole point of the exercise.
RUN mkdir -p crates/boa-proto/src crates/boa-server/src crates/boa-client/src \
    && echo 'pub fn stub() {}' > crates/boa-proto/src/lib.rs \
    && echo 'fn main() {}' > crates/boa-server/src/main.rs \
    && echo 'pub fn stub() {}' > crates/boa-client/src/lib.rs \
    && echo 'fn main() {}' > crates/boa-client/src/main.rs \
    && cargo build --release -p boa-server \
    && rm -rf crates/boa-proto/src crates/boa-server/src crates/boa-client/src \
    && rm -f target/release/boa-server \
    && rm -rf target/release/.fingerprint/boa-proto-* target/release/.fingerprint/boa-server-* \
    && rm -f target/release/deps/libboa_proto* target/release/deps/boa_proto* \
             target/release/deps/boa_server*

# --- the real thing -------------------------------------------------------- #
COPY crates crates
RUN cargo build --release -p boa-server \
    && strip target/release/boa-server

# --------------------------------------------------------------------------- #
FROM debian:bookworm-slim

# `ca-certificates` because the server may be configured with an `https` wormhole rendezvous URL to
# hand to clients, and `curl` so a compose healthcheck has something to call. Nothing else — the
# binary statically links SQLite.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# Not root. The server needs no privileges at all: it binds ports above 1024 and writes to one
# directory.
RUN useradd --system --create-home --uid 10001 boa
COPY --from=build /src/target/release/boa-server /usr/local/bin/boa-server

# Everything that survives a restart lives here: the SQLite file and the attachment blobs. Mount it,
# or a container replacement loses the conversations.
ENV BOA_DATA_DIR=/data
RUN mkdir -p /data && chown boa:boa /data
VOLUME ["/data"]

USER boa
WORKDIR /data

# TCP for chat and control, UDP for voice and screens. **Both** have to be published, and the UDP one
# cannot go through an HTTP reverse proxy — forgetting it produces a server where chat works
# perfectly and nobody can hear anybody.
EXPOSE 8787/tcp
EXPOSE 8788/udp

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/api/info || exit 1

ENTRYPOINT ["boa-server"]

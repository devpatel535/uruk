# Two stages: build with the toolchain, ship without it.
#
#   docker build -f deploy/Dockerfile -t uruk .
#   docker run --rm -p 8080:8080 \
#       -v "$PWD/data:/data:ro" uruk
#
# The data volume is mounted read-only on purpose. `uruk serve` only reads;
# crawling and indexing are separate commands, and running them inside the
# serving container is how a read-only guarantee stops being one.

FROM rust:1.94-slim-bookworm AS build
WORKDIR /src

# Dependencies first, so a source-only change does not rebuild the world.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --locked -p uruk \
    && strip target/release/uruk

# Debian slim rather than scratch or distroless: the binary links glibc and
# needs CA certificates for the crawler's HTTPS. A static musl build would be
# smaller, and would be worth doing if the image size ever mattered more than
# the build being obvious.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# An unprivileged user with no home and no shell. Nothing in here needs either.
RUN useradd --system --no-create-home --shell /usr/sbin/nologin --uid 10001 uruk

COPY --from=build /src/target/release/uruk /usr/local/bin/uruk

USER 10001:10001
WORKDIR /data
EXPOSE 8080

# 0.0.0.0 inside the container, because the container's own network namespace
# is the boundary; publish it with -p and put a proxy in front for TLS.
ENTRYPOINT ["/usr/local/bin/uruk"]
CMD ["serve", "--index", "/data/index", "--crawl", "/data/crawl", "--address", "0.0.0.0:8080"]

# syntax=docker/dockerfile:1

# The Relay's container image: `suru-relay` alone on a distroless base, run as an unprivileged user, reading its
# configuration from /etc/suru-relay/suru-relay.toml and keeping its records in /var/lib/suru-relay. It runs
# `suru-relay run`, and `docker exec <container> suru-relay logins list` reaches the same records, the configuration
# being named in the environment. The image holds no configuration, and nothing in it reaches anywhere but what its
# configuration names and GitHub.
#
# Built from the root of a checkout of the repository:
#
#     docker build --file crates/suru-relay/Dockerfile --tag suru-relay .
#
# The Relay's release instead builds it from the very binaries the release ships, given as the build context
# `binaries` holding linux-amd64/suru-relay and linux-arm64/suru-relay, so nothing is compiled twice:
#
#     docker buildx build --file crates/suru-relay/Dockerfile --platform linux/amd64,linux/arm64 \
#         --build-arg BINARY=release --build-context binaries=dist/image .

# Where the binary comes from: `source`, compiled here, or `release`, as a release built it.
ARG BINARY=source

# The toolchain is the one rust-toolchain.toml pins, which rustup takes up whatever this image carries.
FROM rust:1.98.1-slim-trixie AS source
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --locked --profile dist --package suru-relay \
    && install -D target/dist/suru-relay /out/suru-relay

FROM scratch AS release
ARG TARGETARCH
COPY --from=binaries linux-${TARGETARCH}/suru-relay /out/suru-relay

FROM ${BINARY} AS binary

# The directory the Relay keeps its records in, owned by the user it runs as, so a volume mounted there is too.
FROM --platform=$BUILDPLATFORM debian:trixie-slim AS records
RUN install --directory --owner=65532 --group=65532 --mode=0700 /var/lib/suru-relay

# glibc and the operating system's trust store, which the Relay verifies GitHub against, and nothing else.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=binary /out/suru-relay /usr/local/bin/suru-relay
COPY --from=records --chown=65532:65532 /var/lib/suru-relay /var/lib/suru-relay
ENV SURU_RELAY_CONFIG=/etc/suru-relay/suru-relay.toml
WORKDIR /var/lib/suru-relay
VOLUME ["/var/lib/suru-relay"]
USER nonroot
EXPOSE 8080
# A container runtime stops it with SIGTERM, on which it ends what it carries and writes the lines it owes.
STOPSIGNAL SIGTERM
ENTRYPOINT ["suru-relay"]
CMD ["run"]

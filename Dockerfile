# Minimal Prometheus-exporter image: a fully static musl binary in `scratch`
# (~a few MB), in the style of the standard Prometheus exporters.
# The device speaks plain HTTP (no TLS), so the binary needs no libc/OpenSSL/
# CA certs at runtime — `scratch` is enough.
#
# Builds natively for the image's architecture (amd64 or arm64); with
# `docker buildx --platform linux/amd64,linux/arm64` each arch builds under
# emulation, so no cross-linker is required.

FROM rust:slim AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN RUST_MUSL_TARGET="$(uname -m)-unknown-linux-musl" \
 && rustup target add "$RUST_MUSL_TARGET" \
 && cargo build --release --target "$RUST_MUSL_TARGET" \
      --bin gocoax-exporter --bin gocoax-remediator --bin gocoax \
 && cp "target/$RUST_MUSL_TARGET/release/gocoax-exporter"   /gocoax-exporter \
 && cp "target/$RUST_MUSL_TARGET/release/gocoax-remediator" /gocoax-remediator \
 && cp "target/$RUST_MUSL_TARGET/release/gocoax"            /gocoax

FROM scratch
# All three binaries ship in one image. Default entrypoint is the exporter;
# the remediator service overrides the entrypoint to /gocoax-remediator, and
# one-off CLI use is `--entrypoint /gocoax`.
COPY --from=build /gocoax-exporter   /gocoax-exporter
COPY --from=build /gocoax-remediator /gocoax-remediator
COPY --from=build /gocoax            /gocoax
# Run as a non-root numeric UID (scratch has no /etc/passwd).
USER 10001:10001
EXPOSE 9420 9421
ENTRYPOINT ["/gocoax-exporter"]
CMD ["--config", "/etc/gocoax/config.toml"]

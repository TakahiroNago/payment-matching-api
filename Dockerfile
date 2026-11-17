# Build:  docker compose build        (or: docker build -t payment-matching-api .)
# The queries are checked against .sqlx at build time, so no database is needed to build the image.

# The toolchain is pinned to a minor version, so that the image is built by a compiler the code was tested with.
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
ENV SQLX_OFFLINE=true
COPY . .
# The registry and the target directory are cache mounts: a change to the source recompiles this crate only,
# not its dependencies. The binary is copied out because a cache mount is not part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/payment-matching-api /payment-matching-api

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /payment-matching-api /usr/local/bin/payment-matching-api
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/payment-matching-api"]
CMD ["serve"]

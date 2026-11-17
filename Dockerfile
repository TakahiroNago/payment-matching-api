# Build:  docker compose build        (or: docker build -t payment-matching-api .)
# The queries are checked against .sqlx at build time, so no database is needed to build the image.

FROM rust:1-slim-bookworm AS build
WORKDIR /src
ENV SQLX_OFFLINE=true
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/payment-matching-api /usr/local/bin/payment-matching-api
ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/payment-matching-api"]
CMD ["serve"]

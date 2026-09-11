FROM rust:1.94.1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --locked --release --bin nx-cache-aws && mkdir /spool

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/nx-cache-aws /nx-cache-aws
COPY --from=build --chown=nonroot:nonroot /spool /spool
USER nonroot
ENV TMPDIR=/spool
VOLUME /spool
EXPOSE 3000
ENTRYPOINT ["/nx-cache-aws"]

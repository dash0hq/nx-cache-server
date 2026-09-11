FROM rust:1.94.1-slim-bookworm@sha256:cf9dd0ec73e75f827fe59123fff9dc65af1a1c8363c3c31ee8d7f8ad0b6a5fb2 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
RUN cargo build --locked --release --bin nx-cache-aws && mkdir -m 0700 /spool

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /build/target/release/nx-cache-aws /nx-cache-aws
COPY --from=build --chown=65532:65532 /spool /spool
COPY LICENSE.txt /LICENSE.txt
USER 65532:65532
ENV SPOOL_DIRECTORY=/spool
EXPOSE 3000
ENTRYPOINT ["/nx-cache-aws"]

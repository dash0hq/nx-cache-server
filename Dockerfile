FROM rust:1.94.1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock LICENSE.txt NOTICE ./
COPY src src
RUN cargo build --locked --release --bin nx-cache-aws
COPY licenses licenses
RUN mkdir -p /spool /licenses/nx-cache-server \
    && cp Cargo.lock LICENSE.txt NOTICE /licenses/nx-cache-server/ \
    && find "$CARGO_HOME/registry/src" -type f \( -iname 'LICENSE*' -o -iname 'COPYING*' -o -iname 'NOTICE*' -o -iname 'UNLICENSE*' \) \
       -exec sh -c 'for file do crate=$(basename "$(dirname "$file")"); mkdir -p "/licenses/$crate"; cp "$file" "/licenses/$crate/"; done' sh {} + \
    && for crate in base64-simd-0.8.0 vsimd-0.8.0; do mkdir -p "/licenses/$crate"; cp licenses/Nugine-simd-MIT.txt "/licenses/$crate/"; done \
    && for crate in opentelemetry-0.32.0 opentelemetry-http-0.32.0 opentelemetry-otlp-0.32.0 opentelemetry-proto-0.32.0 opentelemetry_sdk-0.32.1; do mkdir -p "/licenses/$crate"; cp LICENSE.txt "/licenses/$crate/Apache-2.0.txt"; done

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /build/target/release/nx-cache-aws /nx-cache-aws
COPY --from=build --chown=nonroot:nonroot /spool /spool
COPY --from=build /licenses /licenses
USER nonroot
ENV TMPDIR=/spool
VOLUME /spool
EXPOSE 3000
LABEL org.opencontainers.image.source="https://github.com/dash0hq/nx-cache-server" \
      org.opencontainers.image.licenses="Apache-2.0"
ENTRYPOINT ["/nx-cache-aws"]

# Build stage
FROM rust:1.97-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p tinlink-server
# Upload directory owned by the runtime user, so a fresh named volume is writable.
RUN mkdir -p /data/files

# Runtime stage: minimal image without shell, runs as non-root
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/tinlink-server /usr/local/bin/tinlink-server
COPY --from=build --chown=65532:65532 /data /data
COPY --from=build /src/apps/web /app/web
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tinlink-server"]

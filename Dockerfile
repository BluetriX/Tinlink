# Build stage
FROM rust:1.97-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p tinlink-server

# Runtime stage: minimal image without shell, runs as non-root
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/tinlink-server /usr/local/bin/tinlink-server
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/tinlink-server"]

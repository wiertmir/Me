# Build from the repository root: docker build -f auth-service/Dockerfile -t me/auth-service:dev .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY common common
COPY auth-service auth-service
# The cache mounts keep the crate registry and the build directory between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p auth-service && cp target/release/auth-service /auth-service

# glibc, CA certificates and time zones, nothing else (no shell).
FROM gcr.io/distroless/cc-debian12
COPY --from=build /auth-service /usr/local/bin/auth-service
USER 1000
ENTRYPOINT ["auth-service"]
CMD ["/etc/me/config.toml"]

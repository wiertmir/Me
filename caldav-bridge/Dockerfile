# Build from the repository root: docker build -f caldav-bridge/Dockerfile -t me/caldav-bridge:dev .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY common common
COPY auth-service auth-service
COPY calendar-service calendar-service
COPY tasks-service tasks-service
COPY caldav-bridge caldav-bridge
# The cache mounts keep the crate registry and the build directory between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p caldav-bridge && cp target/release/caldav-bridge /caldav-bridge

# glibc, CA certificates and time zones, nothing else (no shell).
FROM gcr.io/distroless/cc-debian12
COPY --from=build /caldav-bridge /usr/local/bin/caldav-bridge
USER 1000
ENTRYPOINT ["caldav-bridge"]
CMD ["/etc/me/caldav.toml"]

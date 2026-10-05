# Build from the repository root: docker build -f calendar-service/Dockerfile -t me/calendar-service:dev .
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
    cargo build --release --locked -p calendar-service && cp target/release/calendar-service /calendar-service

# glibc, CA certificates and time zones, nothing else (no shell).
FROM gcr.io/distroless/cc-debian12
COPY --from=build /calendar-service /usr/local/bin/calendar-service
USER 1000
ENTRYPOINT ["calendar-service"]
CMD ["/etc/me/calendar.toml"]

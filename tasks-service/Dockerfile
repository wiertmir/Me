# Build from the repository root: docker build -f tasks-service/Dockerfile -t me/tasks-service:dev .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY common common
COPY auth-service auth-service
COPY calendar-service calendar-service
COPY tasks-service tasks-service
# The cache mounts keep the crate registry and the build directory between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p tasks-service && cp target/release/tasks-service /tasks-service

# glibc, CA certificates and time zones, nothing else (no shell).
FROM gcr.io/distroless/cc-debian12
COPY --from=build /tasks-service /usr/local/bin/tasks-service
USER 1000
ENTRYPOINT ["tasks-service"]
CMD ["/etc/me/tasks.toml"]

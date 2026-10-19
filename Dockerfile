FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM alpine:3
# The mounted workspace is usually owned by another uid, which git refuses
# to operate on without safe.directory.
RUN apk add --no-cache git && git config --system --add safe.directory '*'
COPY --from=build /src/target/release/git-polite /usr/local/bin/git-polite
WORKDIR /workspace
ENTRYPOINT ["git-polite"]
CMD ["mcp"]

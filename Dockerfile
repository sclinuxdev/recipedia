# Multi-stage build: musl-static server + CLI, runtime needs nothing but git.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release

FROM alpine:3
RUN apk add --no-cache git ca-certificates tini
COPY --from=build /src/target/release/recipedia-server /usr/local/bin/
COPY --from=build /src/target/release/recipedia /usr/local/bin/
ENV RECIPEEDIA_STATE_DIR=/data \
    RECIPEEDIA_LISTEN=0.0.0.0:8300
VOLUME /data
EXPOSE 8300
ENTRYPOINT ["/sbin/tini", "--"]
CMD ["recipedia-server"]

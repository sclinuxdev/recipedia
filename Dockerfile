# Runtime-only image: pulls the released static musl binaries instead of
# compiling. Pin with --build-arg RECIPEEDIA_VERSION=v0.0.2 (no leading v in
# the default keeps older compose files working; the tag always carries it).
ARG RECIPEEDIA_VERSION=0.0.2
FROM alpine:3
ARG RECIPEEDIA_VERSION
RUN set -eux; ver="${RECIPEEDIA_VERSION#v}"; \
    apk add --no-cache git ca-certificates tini curl; \
    curl -fsSL "https://github.com/sclinuxdev/recipedia/releases/download/v${ver}/recipedia-${ver}-x86_64-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin; \
    chmod +x /usr/local/bin/recipedia-server /usr/local/bin/recipedia
ENV RECIPEEDIA_STATE_DIR=/data \
    RECIPEEDIA_LISTEN=0.0.0.0:8300
VOLUME /data
EXPOSE 8300
ENTRYPOINT ["/sbin/tini", "--"]
CMD ["recipedia-server"]

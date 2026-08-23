# Runtime-only image: pulls the released binaries instead of compiling.
# Pin with --build-arg RECIPEEDIA_VERSION=0.1.0 (no leading v in the default
# keeps older compose files working; the tag always carries it).
ARG RECIPEEDIA_VERSION=0.1.0
FROM debian:bookworm-slim
ARG RECIPEEDIA_VERSION
RUN set -eux; ver="${RECIPEEDIA_VERSION#v}"; \
    apt-get update; \
    apt-get install -y --no-install-recommends git ca-certificates tini curl; \
    rm -rf /var/lib/apt/lists/*; \
    curl -fsSL "https://github.com/sclinuxdev/recipedia/releases/download/v${ver}/recipedia-${ver}-x86_64-linux-gnu.tar.gz" \
      | tar -xz -C /usr/local/bin; \
    chmod +x /usr/local/bin/recipedia-server /usr/local/bin/recipedia
ENV RECIPEEDIA_STATE_DIR=/data \
    RECIPEEDIA_LISTEN=0.0.0.0:8300
VOLUME /data
EXPOSE 8300
ENTRYPOINT ["/sbin/tini", "--"]
CMD ["recipedia-server"]

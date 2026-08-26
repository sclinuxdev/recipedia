# Runtime-only image: pulls the released binaries instead of compiling.
# Pin with --build-arg RECIPEEDIA_VERSION=0.1.5 (no leading v in the default
# keeps older compose files working; the tag always carries it).
ARG RECIPEEDIA_VERSION=0.1.5
# ubuntu 24.04: its glibc 2.39 matches what the release binaries were
# compiled against (bookworm's 2.36 is too old).
FROM ubuntu:24.04
ARG RECIPEEDIA_VERSION
# 覆盖同名 release 资产后, 用它击穿下载层缓存:
#   docker build --build-arg CACHE_BUST=$(date +%s) ...
ARG CACHE_BUST=0
RUN set -eux; ver="${RECIPEEDIA_VERSION#v}"; \
    apt-get update; \
    apt-get install -y --no-install-recommends git ca-certificates tini curl; \
    rm -rf /var/lib/apt/lists/*; \
    curl -fsSL "https://github.com/sclinuxdev/recipedia/releases/download/v${ver}/recipedia-${ver}-x86_64-linux-gnu.tar.gz?cb=${CACHE_BUST}" \
      | tar -xz -C /usr/local/bin; \
    chmod +x /usr/local/bin/recipedia-server /usr/local/bin/recipedia
ENV RECIPEEDIA_STATE_DIR=/data \
    RECIPEEDIA_LISTEN=0.0.0.0:8300
VOLUME /data
EXPOSE 8300
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["recipedia-server"]

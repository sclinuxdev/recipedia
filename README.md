# Recipedia

sclinux recipe & repository hub — a read-only web presentation of the
[sclinuxdev/recipes](https://github.com/sclinuxdev/recipes.amd64) git tree, plus
built-in hosting of published binary packages.

## What it does

- **Recipe hub** — syncs the recipes git tree (webhook or poll), parses every
  `recipe.toml`, and serves browsable package pages: metadata, dependency
  graph edges, reverse dependencies, and the full recipe source.
- **Build status** — every package's state (missing / outdated / built /
  ahead) is derived on request as the diff between recipe versions and the
  published table. Nothing is stored; the two tables are the truth.
- **Multi-version recipes** — a package may keep several version directories;
  lists and the status board show the newest, detail pages carry the ladder.
- **Virtual names resolve** — `/package/virtual/libc` doesn't 404: it lists
  the packages whose `provides` cover the name.
- **Package hosting** — `POST /api/repo/publish/{filename}` (bearer token)
  streams a `*.pkg.tar.zst` straight to disk, extracts `.METADATA`, regenerates
  an `index.toml` byte-compatible with sage's channel reader, and serves the
  lot under `/repo/*` with sha256 ETags. Published artifacts can be withdrawn,
  file listings are browsable, and build logs attach to their archive.
- **`recipedia` CLI** — `login`, `publish` (ETag-aware skip, auto-attaches a
  sibling `<archive>.log`), `unpublish`, `status`.

## Running

```sh
# server (env-driven, sensible defaults)
RECIPEEDIA_STATE_DIR=/srv/recipedia RECIPEEDIA_WEBHOOK_SECRET=... recipedia-server
recipedia-server token <label>        # mint a publish token

# builder side
recipedia login https://hub.example.com --token rt_...
recipedia publish target/pkgdir/      # upload everything not yet on the server
recipedia status --state missing
```

Configuration is environment-only: `RECIPEEDIA_LISTEN`, `RECIPEEDIA_DB`,
`RECIPEEDIA_STATE_DIR`, `RECIPEEDIA_GIT_URLS` (comma-separated `arch=url`
pairs; default aggregates recipes.amd64 + recipes.aarch64; legacy singular
`RECIPEEDIA_GIT_URL` still works), `RECIPEEDIA_WEBHOOK_SECRET`,
`RECIPEEDIA_POLL_SECS`, `RECIPEEDIA_REPO_URL` (public repo domain for
frontend file links; unset keeps same-origin `/repo/...`).

## Deploying (Docker / 1Panel)

The repo ships a `Dockerfile` (multi-stage musl build, runtime image is
alpine + git only) and a `compose.yaml`. On a 1Panel box: 容器 → 编排 →
创建编排, paste `compose.yaml` as-is — it builds from this GitHub repo,
binds `127.0.0.1:8300` on the host and keeps all state in the compose
directory's `data/`. Then 网站 → 创建网站 → 反向代理 to `127.0.0.1:8300`
for public HTTPS. Mint publish tokens with
`docker exec recipedia recipedia-server token <label>`.

## Build

```sh
cargo build --release
cargo test && cargo clippy --all-targets
```

Rust + axum + SQLite; the only state is one SQLite file, a read-only git
mirror, and the published package directory.

## License

BSD-2-Clause — see [LICENSE](LICENSE).

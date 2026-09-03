# Recipedia

sclinux recipe & repository hub — a read-only web presentation of the canonical
[sclinuxdev/recipes](https://github.com/sclinuxdev/recipes) git tree, plus
built-in hosting of published binary packages.

## What it does

- **Recipe hub** — syncs the recipes git tree (webhook or poll), parses every
  `recipe.toml`, and serves browsable package pages: metadata, dependency
  graph edges, reverse dependencies, and the full recipe source.
- **Build status** — every package's state (missing / outdated / built /
  ahead) is derived on request as the diff between recipe versions and the
  published table. Nothing is stored; the two tables are the truth.
- **Multi-version recipes** — all matching Sage definitions are retained;
  lists and the status board show the newest, detail pages carry the ladder.
- **Virtual names resolve** — `/package/virtual/libc` doesn't 404: it lists
  the packages whose `provides` cover the name.
- **Package hosting** — `POST /api/repo/publish/{filename}` (bearer token)
  streams a Sage 0.4 `*.pkg.tar.zst` straight to disk, validates it with
  Sage's archive reader, and rebuilds signed `index.mdb`, `index.mdb.zst`, and
  `index.mdb.sig` files below `repo/<subchannel>/`. Published
  artifacts can be withdrawn, file listings are browsable, and build logs
  attach to their archive.
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
`RECIPEEDIA_STATE_DIR`, `RECIPEEDIA_GIT_URL` (one canonical repository;
defaults to `https://github.com/sclinuxdev/recipes`), `RECIPEEDIA_WEBHOOK_SECRET`,
`RECIPEEDIA_POLL_SECS`, `RECIPEEDIA_FRONTEND_URL` (public frontend origin),
`RECIPEEDIA_REPO_URL` (public repo domain for file links; unset keeps
same-origin `/repo/...`), `RECIPEEDIA_REPO_CHANNEL` (default `main`), and
`RECIPEEDIA_REPO_SIGNING_KEY` (raw 32-byte Ed25519 key; generated on first
publish when unset). The matching `.pub` file is written beside the key for
Sage clients' `channels.toml`.

Recipes are discovered recursively and validated with Sage 0.4's
`RecipeSpec`; no legacy category/version-directory convention is required. A
sync diffs the previous and new commits and reports architectures found in the
changed paths in both the webhook response and sync log.

## Deploying (Docker / 1Panel)

The `Dockerfile` is a small Ubuntu 24.04 runtime image: it downloads the
matching x86_64 glibc binaries from GitHub Releases instead of compiling on
the server. Build it only after that release exists:

```sh
docker build --pull --build-arg RECIPEEDIA_VERSION=0.1.6 \
  -t recipedia:0.1.6 -t recipedia:latest \
  https://github.com/sclinuxdev/recipedia.git#v0.1.6
install -d -m 0750 /srv/recipedia
RECIPEEDIA_DATA_DIR=/srv/recipedia docker compose up -d --force-recreate
docker exec recipedia recipedia-server token <label>
```

For 1Panel, run the `docker build` command in the server terminal first, then
open 容器 → 编排 → 创建编排 and paste `compose.yaml`. Set
`RECIPEEDIA_DATA_DIR=/srv/recipedia` in the compose environment so removing or
moving the compose project cannot remove the LMDB state, git mirror, packages, or
logs. Create a reverse proxy from the public site to `127.0.0.1:8300`; if using
the split domains shown in `compose.yaml`, proxy the repository domain to
`http://127.0.0.1:8300/repo/` with the trailing slash. Configure the GitHub
webhook payload URL as `https://<frontend>/api/webhook/github`, content type
`application/json`, and use the same secret as `RECIPEEDIA_WEBHOOK_SECRET`.

To upgrade, replace `0.1.6` with the new release in the build command, retag
`recipedia:latest`, and run `docker compose up -d --force-recreate` again. The
mounted data directory is reused and the disposable recipe cache is rebuilt
from the canonical git source.

## Build

```sh
cargo build --release
cargo test && cargo clippy --all-targets
```

Rust + axum + LMDB; the state is one LMDB directory, a read-only git mirror,
and the published Sage network-repository directories.

## License

BSD-2-Clause — see [LICENSE](LICENSE).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use askama::Template;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use rusqlite::Connection;
use serde::Serialize;

use crate::config::Config;
use crate::db::{self, PackageRow, PublishedRow, SyncEntry};
use crate::graph;
use crate::repo;
use crate::status::{derive, State as BuildState};
use crate::sync;

/// Uploads are streamed straight to disk; the ceiling only guards against
/// runaway requests. Generous because llvm-sized archives are expected.
const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024 * 1024;

pub struct AppState {
    pub db: Mutex<Connection>,
    pub config: Config,
    /// Set while a sync runs so webhook + poll + boot never double-sync.
    pub syncing: AtomicBool,
}

pub type SharedState = Arc<AppState>;

/// Build logs are plain text and only need to scroll back through a failed
/// phase; anything bigger is a mistake (the archive itself is the artifact).
const MAX_LOG_BYTES: usize = 1024 * 1024;
/// Detail pages show a capped file listing so llvm-sized payloads stay sane.
const DETAIL_FILES_SHOWN: usize = 400;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/packages", get(packages))
        // Catch-all: provided names carry a slash (`virtual/libc`) and must
        // resolve to their provider page like any recipe name.
        .route("/package/{*name}", get(package_detail))
        .route("/category/{cat}", get(category))
        .route("/status", get(status_page))
        .route("/graph", get(graph_page))
        .route("/upload", get(upload_page))
        .route("/api/packages", get(api_packages))
        .route("/api/package/{*name}", get(api_package))
        .route("/api/status", get(api_status))
        .route("/api/graph", get(api_graph))
        .route("/api/webhook/github", post(github_webhook))
        .route(
            "/api/repo/publish/{filename}",
            post(publish)
                .delete(unpublish_file)
                .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route(
            "/api/repo/publish/{filename}/log",
            post(upload_log).layer(DefaultBodyLimit::max(MAX_LOG_BYTES)),
        )
        .route("/repo/{*path}", get(repo_file))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PackageView {
    pub name: String,
    pub category: String,
    pub version: String,
    pub release: String,
    pub description: String,
    pub state: BuildState,
    /// Repo-side `version-release` when a build exists (`""` otherwise).
    pub repo_version: String,
    /// When that build landed (unix seconds; 0 when nothing published).
    pub built_at: i64,
}

impl PackageView {
    /// Renderable build time for list pages (empty when nothing published).
    pub fn built_time(&self) -> String {
        if self.built_at > 0 { db::time_hm_pub(self.built_at) } else { String::new() }
    }
    /// True when the repository carries a different version than the recipe.
    pub fn repo_differs(&self) -> bool {
        !self.repo_version.is_empty() && self.repo_version != format!("{}-{}", self.version, self.release)
    }
}

fn package_views(conn: &Connection, filter: impl Fn(&PackageRow) -> bool) -> Result<Vec<PackageView>> {
    let published = published_index(conn)?;
    let rows = db::latest_packages(conn)?;
    Ok(rows
        .iter()
        .filter(|r| filter(r))
        .map(|r| {
            let published = published.get(&r.name);
            PackageView {
                name: r.name.clone(),
                category: r.category.clone(),
                version: r.version.clone(),
                release: r.release.clone(),
                description: r.description.clone(),
                state: derive(
                    &r.version,
                    &r.release,
                    published.map(|p| (p.version.as_str(), p.release.as_str())),
                ),
                repo_version: published
                    .map(|p| format!("{}-{}", p.version, p.release))
                    .unwrap_or_default(),
                built_at: published.map(|p| p.uploaded_at).unwrap_or(0),
            }
        })
        .collect())
}

fn published_index(conn: &Connection) -> Result<HashMap<String, PublishedRow>> {
    Ok(db::published_latest_by_name(conn)?
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect())
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub total: i64,
    pub built: usize,
    pub outdated: usize,
    pub missing: usize,
    pub ahead: usize,
    pub categories: Vec<db::CategoryCount>,
    pub recent_syncs: Vec<SyncEntry>,
    pub commit: String,
}

async fn index(State(state): State<SharedState>) -> Response {
    with_conn(&state, |conn| {
        let views = package_views(conn, |_| true)?;
        let categories = db::categories(conn)?;
        let recent_syncs = db::recent_syncs(conn, 8)?;
        let commit = db::meta_get(conn, "last_commit")?.unwrap_or_default();
        let count = |s: BuildState| views.iter().filter(|v| v.state == s).count();
        let tpl = IndexTemplate {
            total: views.len() as i64,
            built: count(BuildState::Built),
            outdated: count(BuildState::Outdated),
            missing: count(BuildState::Missing),
            ahead: count(BuildState::Ahead),
            categories,
            recent_syncs,
            commit,
        };
        Ok(Html(tpl.render()?).into_response())
    })
}

#[derive(Template)]
#[template(path = "packages.html")]
pub struct PackagesTemplate {
    pub rows: Vec<PackageView>,
    pub q: String,
    pub category: String,
    pub state: String,
    pub total: usize,
}

async fn packages(
    State(app): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let q = params.get("q").cloned().unwrap_or_default();
    let category = params.get("category").cloned().unwrap_or_default();
    let state_filter = params.get("state").cloned().unwrap_or_default();
    with_conn(&app, |conn| {
        let ql = q.to_lowercase();
        // State filtering runs on derived views, so it happens after mapping.
        let mut rows: Vec<PackageView> = package_views(conn, |r| {
            q.is_empty()
                || r.name.to_lowercase().contains(&ql)
                || r.description.to_lowercase().contains(&ql)
        })?;
        rows.retain(|v| {
            (category.is_empty() || v.category == category)
                && (state_filter.is_empty() || v.state.label() == state_filter)
        });
        let total = rows.len();
        let tpl = PackagesTemplate { rows, q, category, state: state_filter, total };
        Ok(Html(tpl.render()?).into_response())
    })
}

/// One entry of the detail page's recipe-version ladder.
#[derive(Debug, Clone)]
pub struct VersionEntry {
    pub version: String,
    pub release: String,
    pub github_url: String,
    pub latest: bool,
}

#[derive(Template)]
#[template(path = "package_detail.html")]
pub struct DetailTemplate {
    pub pkg: PackageRow,
    pub state: BuildState,
    /// Every published build of this package, newest upload first.
    pub published: Vec<PublishedRow>,
    /// Every recipe version kept in the tree, newest first (index 0 is shown
    /// as the headline).
    pub versions: Vec<VersionEntry>,
    pub reverse: Vec<String>,
    pub recipe_toml: String,
    pub github_url: String,
    pub files: Vec<db::FileLine>,
    pub files_total: usize,
    pub has_log: bool,
    pub log_content: String,
    pub log_builder: String,
}

async fn package_detail(
    State(state): State<SharedState>,
    AxPath(name): AxPath<String>,
) -> Response {
    with_conn(&state, |conn| {
        let versions = db::package_versions(conn, &name)?;
        let published = db::published_for_name(conn, &name)?;
        let pkg = match versions.first().cloned() {
            Some(pkg) => pkg,
            None => {
                // Not a recipe name -- but a provided one (`virtual/libc`,
                // `cc`, ...): list the providers instead of dead-ending.
                let provider_names = db::providers(conn, &name)?;
                if !provider_names.is_empty() {
                    return render_virtual(conn, &name, &provider_names);
                }
                // Published but recipeless (recipe removed / pre-tree build):
                // describe it from its own manifest meta.
                match published.first() {
                    Some(row) => orphan_row(conn, row)?,
                    None => return Ok(StatusCode::NOT_FOUND.into_response()),
                }
            }
        };
        // State compares against the newest *version* on the repo side, not
        // merely the most recent upload (an old rebuild must not flip it).
        let best_pub = published.iter().max_by(|a, b| {
            crate::status::compare_versions(
                &format!("{}-{}", a.version, a.release),
                &format!("{}-{}", b.version, b.release),
            )
            .then(a.uploaded_at.cmp(&b.uploaded_at))
        });
        let st = derive(
            &pkg.version,
            &pkg.release,
            best_pub.map(|p| (p.version.as_str(), p.release.as_str())),
        );
        let ladder = versions
            .iter()
            .enumerate()
            .map(|(i, v)| VersionEntry {
                version: v.version.clone(),
                release: v.release.clone(),
                github_url: github_blob_url(&state.config.git_url, &v.recipe_path),
                latest: i == 0,
            })
            .collect();
        // Files and log come from the representative archive.
        let (files, files_total, log) = match best_pub {
            Some(row) => {
                let all = db::file_list(conn, &row.filename)?;
                let total = all.len();
                (all.into_iter().take(DETAIL_FILES_SHOWN).collect(), total, db::log_get(conn, &row.filename)?)
            }
            None => (Vec::new(), 0, None),
        };
        let recipe_toml = std::fs::read_to_string(state.config.git_dir().join(&pkg.recipe_path))
            .unwrap_or_else(|_| "<recipe not on disk>".to_string());
        let github_url = github_blob_url(&state.config.git_url, &pkg.recipe_path);
        let reverse = db::reverse_deps(conn, &name)?;
        let tpl = DetailTemplate {
            state: st,
            published,
            versions: ladder,
            files,
            files_total,
            has_log: log.is_some(),
            log_content: log.as_ref().map(|l| l.content.clone()).unwrap_or_default(),
            log_builder: log.as_ref().map(|l| l.builder.clone()).unwrap_or_default(),
            pkg,
            reverse,
            recipe_toml,
            github_url,
        };
        Ok(Html(tpl.render()?).into_response())
    })
}

#[derive(Debug, Clone)]
pub struct ProviderView {
    pub name: String,
    pub state: BuildState,
    pub repo_version: String,
}

/// A name that exists only as someone's `provides` entry (`virtual/libc`,
/// `cc`): no recipe of its own, so the page resolves it to its providers.
#[derive(Template)]
#[template(path = "virtual.html")]
pub struct VirtualTemplate {
    pub name: String,
    pub providers: Vec<ProviderView>,
}

fn render_virtual(conn: &Connection, name: &str, provider_names: &[String]) -> Result<Response> {
    let wanted: std::collections::HashSet<&str> =
        provider_names.iter().map(String::as_str).collect();
    let providers = package_views(conn, |r| wanted.contains(r.name.as_str()))?
        .into_iter()
        .map(|v| ProviderView { name: v.name, state: v.state, repo_version: v.repo_version })
        .collect();
    Ok(Html(VirtualTemplate { name: name.to_string(), providers }.render()?).into_response())
}

/// A pseudo-recipe row for a package that exists only in the repository:
/// identity and metadata come from its manifest, dependency fields stay empty.
fn orphan_row(conn: &Connection, row: &PublishedRow) -> Result<PackageRow> {
    let meta = db::published_meta(conn, &row.filename)?;
    Ok(PackageRow {
        name: row.name.clone(),
        category: "orphan".into(),
        version: meta.as_ref().map(|m| m.version.clone()).unwrap_or_else(|| row.version.clone()),
        release: meta.as_ref().map(|m| m.release.clone()).unwrap_or_else(|| row.release.clone()),
        description: meta.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
        license: meta.as_ref().map(|m| m.license.clone()).unwrap_or_default(),
        channel: meta.as_ref().map(|m| m.channel.clone()).unwrap_or_default(),
        provides: meta.as_ref().map(|m| m.provides.clone()).unwrap_or_default(),
        dependencies: Vec::new(),
        build_dependencies: Vec::new(),
        conffiles: meta.as_ref().map(|m| m.conffiles.clone()).unwrap_or_default(),
        source_url: String::new(),
        source_sha256: String::new(),
        recipe_path: String::new(),
    })
}

#[derive(Template)]
#[template(path = "packages.html")]
pub struct CategoryTemplate {
    pub rows: Vec<PackageView>,
    pub q: String,
    pub category: String,
    pub state: String,
    pub total: usize,
}

async fn category(State(state): State<SharedState>, AxPath(cat): AxPath<String>) -> Response {
    with_conn(&state, |conn| {
        let rows = package_views(conn, |r| r.category == cat)?;
        let total = rows.len();
        let tpl = CategoryTemplate {
            rows,
            q: String::new(),
            category: cat,
            state: String::new(),
            total,
        };
        Ok(Html(tpl.render()?).into_response())
    })
}

#[derive(Template)]
#[template(path = "status.html")]
pub struct StatusTemplate {
    pub missing: Vec<PackageView>,
    pub outdated: Vec<PackageView>,
    pub ahead: Vec<PackageView>,
    pub built: usize,
}

async fn status_page(State(state): State<SharedState>) -> Response {
    with_conn(&state, |conn| {
        let views = package_views(conn, |_| true)?;
        let mut missing: Vec<_> = views.iter().filter(|v| v.state == BuildState::Missing).cloned().collect();
        let mut outdated: Vec<_> = views.iter().filter(|v| v.state == BuildState::Outdated).cloned().collect();
        let ahead: Vec<_> = views.iter().filter(|v| v.state == BuildState::Ahead).cloned().collect();
        let built = views.iter().filter(|v| v.state == BuildState::Built).count();
        missing.sort_by(|a, b| a.name.cmp(&b.name));
        outdated.sort_by(|a, b| a.name.cmp(&b.name));
        let tpl = StatusTemplate { missing, outdated, ahead, built };
        Ok(Html(tpl.render()?).into_response())
    })
}

#[derive(Template)]
#[template(path = "upload.html")]
pub struct UploadTemplate;

async fn upload_page() -> Response {
    Html(UploadTemplate.render().expect("static template")).into_response()
}

// ---------------------------------------------------------------------------
// Dependency graph
// ---------------------------------------------------------------------------

/// Layered layout over the latest-recipe dependency edges; node color carries
/// the derived build state.
fn compute_graph(conn: &Connection) -> Result<(graph::Graph, usize)> {
    let rows = db::latest_packages(conn)?;
    let published: HashMap<String, (String, String)> = db::published_latest_by_name(conn)?
        .into_iter()
        .map(|p| (p.name, (p.version, p.release)))
        .collect();
    let total = rows.len();
    Ok((graph::build(&rows, &published), total))
}

#[derive(Template)]
#[template(path = "graph.html")]
pub struct GraphTemplate {
    pub svg: String,
    pub node_count: usize,
    pub edge_count: usize,
}

async fn graph_page(State(state): State<SharedState>) -> Response {
    with_conn(&state, |conn| {
        let (g, total) = compute_graph(conn)?;
        let tpl = GraphTemplate { svg: g.render_svg(), node_count: total, edge_count: g.edges.len() };
        Ok(Html(tpl.render()?).into_response())
    })
}

async fn api_graph(State(state): State<SharedState>) -> Response {
    with_conn(&state, |conn| {
        let (g, _) = compute_graph(conn)?;
        let nodes: Vec<serde_json::Value> = g
            .nodes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "name": n.name, "level": n.level, "state": n.state.label(),
                })
            })
            .collect();
        let edges: Vec<serde_json::Value> = g
            .edges
            .iter()
            .map(|&(a, b)| serde_json::json!([g.nodes[a].name, g.nodes[b].name]))
            .collect();
        Ok(json_response(&serde_json::json!({
            "width": g.width, "height": g.height, "nodes": nodes, "edges": edges,
        })))
    })
}

// ---------------------------------------------------------------------------
// Publish (Bearer token) & static repository
// ---------------------------------------------------------------------------

/// Resolve the request's bearer token to a builder label, or produce the 401.
fn authorize(state: &SharedState, headers: &HeaderMap) -> Result<String, Box<Response>> {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(token) = presented else {
        return Err(Box::new((StatusCode::UNAUTHORIZED, "missing bearer token").into_response()));
    };
    match db::token_label(&state.db.lock().expect("db mutex poisoned"), token) {
        Ok(Some(label)) => Ok(label),
        Ok(None) => Err(Box::new((StatusCode::UNAUTHORIZED, "invalid token").into_response())),
        Err(e) => Err(Box::new((StatusCode::INTERNAL_SERVER_ERROR, format!("token check failed: {e:#}")).into_response())),
    }
}

async fn publish(
    State(state): State<SharedState>,
    AxPath(filename): AxPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let builder = match authorize(&state, &headers) {
        Ok(label) => label,
        Err(resp) => return *resp,
    };
    if !repo::valid_filename(&filename) {
        return (StatusCode::BAD_REQUEST, "invalid package filename").into_response();
    }
    let declared = headers
        .get("x-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    std::fs::create_dir_all(&state.config.repo_dir).ok();
    let tmp_path = state
        .config
        .repo_dir
        .join(format!(".incoming-{}-{}", std::process::id(), filename));
    let mut file = match tokio::fs::File::create(&tmp_path).await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot stage upload: {e}"))
                .into_response()
        }
    };

    // Stream body to disk while hashing — archives are far too big to buffer.
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    let mut hasher = sha2::Sha256::new();
    let mut stream = body.into_data_stream();
    let mut aborted: Option<String> = None;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                hasher.update(&bytes);
                if let Err(e) = file.write_all(&bytes).await {
                    aborted = Some(format!("cannot write upload: {e}"));
                    break;
                }
            }
            Err(e) => {
                aborted = Some(format!("upload interrupted: {e}"));
                break;
            }
        }
    }
    drop(file);
    if let Some(msg) = aborted {
        tokio::fs::remove_file(&tmp_path).await.ok();
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }

    let sha256 = hex::encode(hasher.finalize());
    let config = state.config.clone();
    let ingest_name = filename.clone();
    let staged = tmp_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.db.lock().expect("publish worker: db mutex poisoned");
        repo::ingest(&conn, &config, &staged, &ingest_name, &sha256, declared.as_deref(), &builder)
    })
    .await;
    match result {
        Ok(Ok(receipt)) => json_response(&receipt),
        Ok(Err(e)) => {
            tokio::fs::remove_file(&tmp_path).await.ok();
            (StatusCode::UNPROCESSABLE_ENTITY, format!("publish rejected: {e:#}")).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("publish task panicked: {e}")).into_response(),
    }
}

/// Withdraw a published artifact (token required): DB rows, build log, file,
/// re-index. The repository stays a curated space, not an append-only dump.
async fn unpublish_file(
    State(state): State<SharedState>,
    AxPath(filename): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let builder = match authorize(&state, &headers) {
        Ok(label) => label,
        Err(resp) => return *resp,
    };
    if !repo::valid_filename(&filename) {
        return (StatusCode::BAD_REQUEST, "invalid package filename").into_response();
    }
    let exists = {
        let conn = state.db.lock().expect("db mutex poisoned");
        db::published_sha256(&conn, &filename)
    };
    match exists {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, format!("{filename} is not published")).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("database error: {e:#}")).into_response(),
    }
    let config = state.config.clone();
    let target = filename.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = state.db.lock().expect("unpublish worker: db mutex poisoned");
        repo::unpublish(&conn, &config, &target)
    })
    .await;
    match result {
        Ok(Ok(())) => json_response(&serde_json::json!({"deleted": filename, "by": builder})),
        Ok(Err(e)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("unpublish failed: {e:#}"),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task panicked: {e}")).into_response(),
    }
}

/// Attach/replace a plain-text build log for a published archive. The CLI
/// uploads `<archive>.log` siblings automatically after a successful publish.
async fn upload_log(
    State(state): State<SharedState>,
    AxPath(filename): AxPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let builder = match authorize(&state, &headers) {
        Ok(label) => label,
        Err(resp) => return *resp,
    };
    if !repo::valid_filename(&filename) {
        return (StatusCode::BAD_REQUEST, "invalid package filename").into_response();
    }
    if body.len() > MAX_LOG_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "log exceeds 1 MiB").into_response();
    }
    let content = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "log must be UTF-8 text").into_response(),
    };
    let target = filename.clone();
    let result: Result<usize> = (|| {
        let conn = state.db.lock().expect("db mutex poisoned");
        // Only logs for things actually published: no orphans, no squatting.
        match db::published_sha256(&conn, &target)? {
            Some(_) => db::log_upsert(&conn, &target, content.trim_end(), &builder).map(|_| body.len()),
            None => anyhow::bail!("{target} is not published"),
        }
    })();
    match result {
        Ok(len) => json_response(&serde_json::json!({"stored": len, "filename": filename})),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("log upload rejected: {e:#}"),
        )
            .into_response(),
    }
}


/// Static delivery of published packages and index.toml. ETag carries the
/// stored sha256 so clients can skip re-uploads of unchanged files.
async fn repo_file(
    State(state): State<SharedState>,
    AxPath(path): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    if path.split('/').any(|seg| seg.is_empty() || seg == ".." || seg == ".") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let full = state.config.repo_dir.join(&path);
    let meta = match tokio::fs::metadata(&full).await {
        Ok(m) if m.is_file() => m,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let etag = if path == "index.toml" {
        None // regenerated on every publish; no stable hash worth caching
    } else {
        match db::published_sha256(&state.db.lock().expect("db mutex poisoned"), &path) {
            Ok(Some(sha)) => Some(sha),
            _ => None,
        }
    };
    if let Some(sha) = &etag {
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|want| want.trim() == format!("\"{sha}\""))
        {
            return (
                StatusCode::NOT_MODIFIED,
                [("etag", format!("\"{sha}\""))],
            )
                .into_response();
        }
    }

    let file = match tokio::fs::File::open(&full).await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot open {}: {e}", path))
                .into_response()
        }
    };
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, meta.len());
    if let Some(sha) = &etag {
        builder = builder.header(header::ETAG, format!("\"{sha}\""));
    }
    let stream = tokio_util::io::ReaderStream::new(file);
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// JSON API
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ApiStatusEntry {
    name: String,
    recipe_version: String,
    recipe_release: String,
    repo_version: Option<String>,
    repo_release: Option<String>,
    state: BuildState,
}

async fn api_packages(State(app): State<SharedState>) -> Response {
    with_conn(&app, |conn| {
        let pkgs = db::all_packages(conn)?;
        Ok(json_response(&pkgs))
    })
}

async fn api_package(State(state): State<SharedState>, AxPath(name): AxPath<String>) -> Response {
    with_conn(&state, |conn| {
        let Some(pkg) = db::package_by_name(conn, &name)? else {
            // Resolve provided names (`virtual/libc`, `cc`, ...) to providers.
            let provider_names = db::providers(conn, &name)?;
            if provider_names.is_empty() {
                return Ok(StatusCode::NOT_FOUND.into_response());
            }
            return Ok(json_response(&serde_json::json!({
                "name": name, "virtual": true, "providers": provider_names,
            })));
        };
        let published_idx = published_index(conn)?;
        let published = published_idx.get(&name);
        let entry = ApiStatusEntry {
            name: pkg.name.clone(),
            state: derive(
                &pkg.version,
                &pkg.release,
                published.map(|p| (p.version.as_str(), p.release.as_str())),
            ),
            recipe_version: pkg.version.clone(),
            recipe_release: pkg.release.clone(),
            repo_version: published.map(|p| p.version.clone()),
            repo_release: published.map(|p| p.release.clone()),
        };
        let versions: Vec<serde_json::Value> = db::package_versions(conn, &name)?
            .iter()
            .map(|v| serde_json::json!({"version": v.version, "release": v.release, "recipe_path": v.recipe_path}))
            .collect();
        let body = serde_json::to_string(&(pkg, entry, versions))?;
        Ok(([("content-type", "application/json")], body).into_response())
    })
}

async fn api_status(State(app): State<SharedState>) -> Response {
    with_conn(&app, |conn| {
        let published = published_index(conn)?;
        let entries: Vec<ApiStatusEntry> = db::all_packages(conn)?
            .iter()
            .map(|r| {
                let published = published.get(&r.name);
                ApiStatusEntry {
                    name: r.name.clone(),
                    state: derive(
                        &r.version,
                        &r.release,
                        published.map(|p| (p.version.as_str(), p.release.as_str())),
                    ),
                    recipe_version: r.version.clone(),
                    recipe_release: r.release.clone(),
                    repo_version: published.map(|p| p.version.clone()),
                    repo_release: published.map(|p| p.release.clone()),
                }
            })
            .collect();
        Ok(json_response(&entries))
    })
}

fn json_response<T: Serialize>(value: &T) -> Response {
    match serde_json::to_string(value) {
        Ok(body) => ([("content-type", "application/json")], body).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialization failed: {e}"),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Webhook
// ---------------------------------------------------------------------------

async fn github_webhook(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(secret) = state.config.webhook_secret.clone() else {
        return (StatusCode::FORBIDDEN, "webhook not configured").into_response();
    };
    let Some(signature) = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::UNAUTHORIZED, "missing signature").into_response();
    };
    if !verify_hmac(&secret, signature, &body) {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    trigger_sync(state, "webhook").await
}

/// Shared by the webhook and the poll loop: runs at most one sync at a time.
pub async fn trigger_sync(state: SharedState, trigger: &'static str) -> Response {
    if state.syncing.swap(true, Ordering::SeqCst) {
        return (StatusCode::OK, "sync already in progress").into_response();
    }
    let config = state.config.clone();
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = worker_state.db.lock().expect("sync worker: db mutex poisoned");
        sync::run_sync(&conn, &config.git_url, &config.git_dir(), trigger)
    })
    .await;
    state.syncing.store(false, Ordering::SeqCst);
    match result {
        Ok(Ok(count)) => (StatusCode::OK, format!("synced {count} recipes")).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("sync failed: {err:#}"),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("sync task panicked: {err}"),
        )
            .into_response(),
    }
}

fn verify_hmac(secret: &str, signature: &str, body: &[u8]) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Some(hex_digest) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    // Length-checked full compare: no early exit on content.
    expected.len() == hex_digest.len()
        && expected
            .bytes()
            .zip(hex_digest.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn with_conn<T>(state: &SharedState, f: impl FnOnce(&Connection) -> Result<T>) -> Response
where
    T: IntoResponse,
{
    let conn = state.db.lock().expect("db mutex poisoned");
    match f(&conn) {
        Ok(response) => response.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("database error: {e:#}"),
        )
            .into_response(),
    }
}

fn github_blob_url(git_url: &str, recipe_path: &str) -> String {
    let base = git_url.trim_end_matches(".git");
    format!("{base}/blob/main/{recipe_path}")
}

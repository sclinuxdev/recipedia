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

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/packages", get(packages))
        .route("/package/{name}", get(package_detail))
        .route("/category/{cat}", get(category))
        .route("/status", get(status_page))
        .route("/upload", get(upload_page))
        .route("/api/packages", get(api_packages))
        .route("/api/package/{name}", get(api_package))
        .route("/api/status", get(api_status))
        .route("/api/webhook/github", post(github_webhook))
        .route(
            "/api/repo/publish/{filename}",
            post(publish).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
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
}

fn package_views(conn: &Connection, filter: impl Fn(&PackageRow) -> bool) -> Result<Vec<PackageView>> {
    let published = published_index(conn)?;
    let rows = db::all_packages(conn)?;
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

#[derive(Template)]
#[template(path = "package_detail.html")]
pub struct DetailTemplate {
    pub pkg: PackageRow,
    pub state: BuildState,
    pub published: Option<PublishedRow>,
    pub reverse: Vec<String>,
    pub recipe_toml: String,
    pub github_url: String,
}

async fn package_detail(
    State(state): State<SharedState>,
    AxPath(name): AxPath<String>,
) -> Response {
    with_conn(&state, |conn| {
        let Some(pkg) = db::package_by_name(conn, &name)? else {
            return Ok(StatusCode::NOT_FOUND.into_response());
        };
        let published_idx = published_index(conn)?;
        let published = published_idx.get(&name).cloned();
        let st = derive(
            &pkg.version,
            &pkg.release,
            published.as_ref().map(|p| (p.version.as_str(), p.release.as_str())),
        );
        let recipe_toml = std::fs::read_to_string(state.config.git_dir().join(&pkg.recipe_path))
            .unwrap_or_else(|_| "<recipe not on disk>".to_string());
        let github_url = github_blob_url(&state.config.git_url, &pkg.recipe_path);
        let reverse = db::reverse_deps(conn, &name)?;
        let tpl = DetailTemplate { pkg, state: st, published, reverse, recipe_toml, github_url };
        Ok(Html(tpl.render()?).into_response())
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
            return Ok(StatusCode::NOT_FOUND.into_response());
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
        let body = serde_json::to_string(&(pkg, entry))?;
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

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
        .route("/about", get(about_page))
        .route("/upload", get(upload_page))
        .route("/api/packages", get(api_packages))
        .route("/api/package/{*name}", get(api_package))
        .route("/api/status", get(api_status))
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
        .route("/repo", get(repo_index))
        .route("/repo/", get(repo_index))
        .route("/repo/{*path}", get(repo_file))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// View models
// ---------------------------------------------------------------------------

/// Navigation links shared by every page. Frontend pages live on the main
/// site origin (`RECIPEEDIA_FRONTEND_URL`); the repository browser may be
/// exposed on its own domain (`RECIPEEDIA_REPO_URL`). Empty bases keep
/// same-origin root-relative links so a dev checkout just works.
#[derive(Debug, Clone)]
pub struct Nav {
    pub home: String,
    pub packages: String,
    pub status: String,
    pub upload: String,
    pub repo: String,
    pub about: String,
}

impl Nav {
    /// Serving build version for the footer.
    pub fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    pub fn from_config(config: &Config) -> Self {
        let fe = config.frontend_url.as_str();
        let repo = if config.repo_base.is_empty() {
            "/repo".to_string()
        } else {
            config.repo_base.clone()
        };
        Nav {
            home: format!("{fe}/"),
            packages: format!("{fe}/packages"),
            status: format!("{fe}/status"),
            upload: format!("{fe}/upload"),
            about: format!("{fe}/about"),
            repo,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackageView {
    pub name: String,
    /// Effective architecture of the recipe (amd64 / aarch64 / any).
    pub arch: String,
    /// URL of the recipes tree this row came from.
    pub origin: String,
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
    pub fn published_time(&self) -> String {
        if self.built_at > 0 {
            db::time_hm_pub(self.built_at)
        } else {
            String::new()
        }
    }
    /// Same instant as UTC ISO-8601 for `<time datetime>` (client-side
    /// timezone conversion); empty when nothing published.
    pub fn published_iso(&self) -> String {
        if self.built_at > 0 {
            db::time_utc(self.built_at)
        } else {
            String::new()
        }
    }
    /// True when the repository carries a different version than the recipe.
    pub fn repo_differs(&self) -> bool {
        !self.repo_version.is_empty()
            && self.repo_version != format!("{}-{}", self.version, self.release)
    }
}

fn package_views(
    conn: &Connection,
    filter: impl Fn(&PackageRow) -> bool,
) -> Result<Vec<PackageView>> {
    let published = published_by_name(conn)?;
    let rows = db::latest_packages(conn)?;
    Ok(rows
        .iter()
        .filter(|r| filter(r))
        .map(|r| {
            let published =
                pick_published(published.get(&r.name).map(Vec::as_slice), &r.arch, None);
            PackageView {
                name: r.name.clone(),
                arch: r.arch.clone(),
                origin: r.origin.clone(),
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

/// Architecture compatibility: a recipe matches its own arch, an `any`
/// recipe is satisfied by any build, and a published `-any` artifact serves
/// every recipe arch.
fn arch_match(recipe_arch: &str, pub_arch: &str) -> bool {
    recipe_arch == pub_arch || recipe_arch == "any" || pub_arch == "any"
}

/// All latest builds for one name (canonicalized arch per row).
fn published_by_name(conn: &Connection) -> Result<HashMap<String, Vec<PublishedRow>>> {
    let mut out: HashMap<String, Vec<PublishedRow>> = HashMap::new();
    for mut row in db::published_latest_by_arch(conn)? {
        row.arch = crate::model::canonical_arch(&row.arch).to_string();
        out.entry(row.name.clone()).or_default().push(row);
    }
    Ok(out)
}

/// Choose the published build to compare against: architecture-compatible
/// rows only; prefer an exact-arch build over `-any`; within that, prefer
/// `want_same_version` when given, else the newest.
fn pick_published<'a>(
    rows: Option<&'a [PublishedRow]>,
    recipe_arch: &str,
    want_same_version: Option<&str>,
) -> Option<&'a PublishedRow> {
    let rows = rows?;
    let mut compatible: Vec<&PublishedRow> = rows
        .iter()
        .filter(|p| arch_match(recipe_arch, &p.arch))
        .collect();
    if compatible.is_empty() {
        return None;
    }
    if let Some(ver) = want_same_version {
        // Exact-arch same-version beats an any-arch same-version.
        if let Some(hit) = compatible
            .iter()
            .find(|p| p.arch == recipe_arch && p.version == ver)
        {
            return Some(*hit);
        }
        if let Some(hit) = compatible.iter().find(|p| p.version == ver) {
            return Some(*hit);
        }
    }
    compatible.sort_by(|a, b| {
        crate::status::compare_versions(
            &format!("{}-{}", b.version, b.release),
            &format!("{}-{}", a.version, a.release),
        )
        .then(b.uploaded_at.cmp(&a.uploaded_at))
    });
    compatible.sort_by_key(|p| p.arch != recipe_arch);
    compatible.first().copied()
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
    pub nav: Nav,
}

async fn index(State(state): State<SharedState>) -> Response {
    with_conn(&state, |conn| {
        let views = package_views(conn, |_| true)?;
        let categories = db::categories(conn)?;
        let recent_syncs = db::recent_syncs(conn, 8)?;
        let commit = commits_summary(conn)?;
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
            nav: Nav::from_config(&state.config),
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
    pub arch: String,
    /// Distinct architectures present across all packages, for the filter.
    pub arches: Vec<String>,
    pub total: usize,
    pub nav: Nav,
}

async fn packages(
    State(app): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let q = params.get("q").cloned().unwrap_or_default();
    let category = params.get("category").cloned().unwrap_or_default();
    let state_filter = params.get("state").cloned().unwrap_or_default();
    let arch_filter = params.get("arch").cloned().unwrap_or_default();
    with_conn(&app, |conn| {
        let ql = q.to_lowercase();
        // State filtering runs on derived views, so it happens after mapping.
        let mut rows: Vec<PackageView> = package_views(conn, |r| {
            (arch_filter.is_empty() || r.arch == arch_filter)
                && (q.is_empty()
                    || r.name.to_lowercase().contains(&ql)
                    || r.description.to_lowercase().contains(&ql))
        })?;
        rows.retain(|v| {
            (category.is_empty() || v.category == category)
                && (state_filter.is_empty() || v.state.label() == state_filter)
        });
        let total = rows.len();
        let arches: Vec<String> = {
            let mut a: Vec<String> = db::latest_packages(conn)?
                .iter()
                .map(|r| r.arch.clone())
                .collect();
            a.sort();
            a.dedup();
            a
        };
        let tpl = PackagesTemplate {
            rows,
            q,
            category,
            state: state_filter,
            arch: arch_filter.clone(),
            arches,
            total,
            nav: Nav::from_config(&app.config),
        };
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
    /// Other architectures this package also exists for, linking to their
    /// own view (`("aarch64", "/package/zlib?arch=aarch64")`).
    pub other_arches: Vec<(String, String)>,
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
    /// Representative build ships a universal service definition.
    pub p_daemon: bool,
    pub has_log: bool,
    pub log_content: String,
    pub log_builder: String,
    pub nav: Nav,
}

async fn package_detail(
    State(state): State<SharedState>,
    AxPath(name): AxPath<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    with_conn(&state, |conn| {
        let want_arch = params
            .get("arch")
            .map(|a| crate::model::canonical_arch(a).to_string());
        let all_versions = db::package_versions(conn, &name)?;
        // One logical package can exist for several architectures; each arch
        // is its own recipe/build identity with its own page view.
        let mut arches: Vec<String> = all_versions.iter().map(|v| v.arch.clone()).collect();
        arches.sort();
        arches.dedup();
        // Requested arch wins; otherwise the tree with the newest recipe.
        let selected_arch = want_arch
            .clone()
            .filter(|a| arches.contains(a))
            .or_else(|| arches.first().cloned());
        let versions: Vec<PackageRow> = match &selected_arch {
            Some(arch) => all_versions
                .iter()
                .filter(|v| &v.arch == arch)
                .cloned()
                .collect(),
            None => all_versions,
        };
        let published_all = db::published_for_name(conn, &name)?;
        let published: Vec<PublishedRow> = match &selected_arch {
            Some(arch) => published_all
                .iter()
                .filter(|p| {
                    let pa = crate::model::canonical_arch(&p.arch);
                    pa == arch || pa == "any" || arch == "any"
                })
                .cloned()
                .collect(),
            None => published_all,
        };
        let pkg = match versions.first().cloned() {
            Some(pkg) => pkg,
            None => {
                // Not a recipe name -- but a provided one (`virtual/libc`,
                // `cc`, ...): list the providers instead of dead-ending.
                let provider_names = db::providers(conn, &name)?;
                if !provider_names.is_empty() {
                    return render_virtual(
                        conn,
                        &name,
                        &provider_names,
                        Nav::from_config(&state.config),
                    );
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
                github_url: github_blob_url(&v.origin, &v.recipe_path),
                latest: i == 0,
            })
            .collect();
        // Files and log come from the representative archive.
        let (files, files_total, log) = match best_pub {
            Some(row) => {
                let all = db::file_list(conn, &row.filename)?;
                let total = all.len();
                (
                    all.into_iter().take(DETAIL_FILES_SHOWN).collect(),
                    total,
                    db::log_get(conn, &row.filename)?,
                )
            }
            None => (Vec::new(), 0, None),
        };
        let recipe_toml = std::fs::read_to_string(state.config.git_dir().join(&pkg.recipe_path))
            .unwrap_or_else(|_| "<recipe not on disk>".to_string());
        let github_url = github_blob_url(&pkg.origin, &pkg.recipe_path);
        let reverse = db::reverse_deps(conn, &name)?;
        let base = format!("/package/{}", name);
        let other_arches = arches
            .iter()
            .filter(|a| Some(a.to_string()) != selected_arch)
            .map(|a| (a.clone(), format!("{}?arch={}", base, a)))
            .collect();
        let p_daemon = best_pub.is_some_and(|r| r.is_daemon());
        let tpl = DetailTemplate {
            other_arches,
            state: st,
            published,
            p_daemon,
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
            nav: Nav::from_config(&state.config),
        };
        Ok(Html(tpl.render()?).into_response())
    })
}

#[derive(Debug, Clone)]
pub struct ProviderView {
    pub name: String,
    pub arch: String,
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
    pub nav: Nav,
}

fn render_virtual(
    conn: &Connection,
    name: &str,
    provider_names: &[String],
    nav: Nav,
) -> Result<Response> {
    let wanted: std::collections::HashSet<&str> =
        provider_names.iter().map(String::as_str).collect();
    let providers = package_views(conn, |r| wanted.contains(r.name.as_str()))?
        .into_iter()
        .map(|v| ProviderView {
            name: v.name,
            arch: v.arch.clone(),
            state: v.state,
            repo_version: v.repo_version,
        })
        .collect();
    Ok(Html(
        VirtualTemplate {
            name: name.to_string(),
            providers,
            nav,
        }
        .render()?,
    )
    .into_response())
}

/// A pseudo-recipe row for a package that exists only in the repository:
/// identity and metadata come from its manifest, dependency fields stay empty.
fn orphan_row(conn: &Connection, row: &PublishedRow) -> Result<PackageRow> {
    let meta = db::published_meta(conn, &row.filename)?;
    Ok(PackageRow {
        name: row.name.clone(),
        arch: crate::model::canonical_arch(&row.arch).to_string(),
        origin: String::new(),
        category: "orphan".into(),
        version: meta
            .as_ref()
            .map(|m| m.version.clone())
            .unwrap_or_else(|| row.version.clone()),
        release: meta
            .as_ref()
            .map(|m| m.release.clone())
            .unwrap_or_else(|| row.release.clone()),
        description: meta
            .as_ref()
            .map(|m| m.description.clone())
            .unwrap_or_default(),
        license: meta.as_ref().map(|m| m.license.clone()).unwrap_or_default(),
        channel: meta.as_ref().map(|m| m.channel.clone()).unwrap_or_default(),
        provides: meta
            .as_ref()
            .map(|m| m.provides.clone())
            .unwrap_or_default(),
        dependencies: Vec::new(),
        build_dependencies: Vec::new(),
        conffiles: meta
            .as_ref()
            .map(|m| m.conffiles.clone())
            .unwrap_or_default(),
        source_url: String::new(),
        source_sha256: String::new(),
        upstream_url: String::new(),
        upstream_version_regex: String::new(),
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
    /// Unused by category pages (the filter form only shows on /packages);
    /// present because both share one template file.
    pub arch: String,
    pub arches: Vec<String>,
    pub total: usize,
    pub nav: Nav,
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
            arch: String::new(),
            arches: Vec::new(),
            total,
            nav: Nav::from_config(&state.config),
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
    pub arch: String,
    /// Distinct architectures present across all packages, for the filter.
    pub arches: Vec<String>,
    pub nav: Nav,
}

async fn status_page(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let arch_filter = params.get("arch").cloned().unwrap_or_default();
    with_conn(&state, |conn| {
        let mut arches: Vec<String> = db::latest_packages(conn)?
            .iter()
            .map(|r| r.arch.clone())
            .collect();
        arches.sort();
        arches.dedup();
        let views = package_views(conn, |r| arch_filter.is_empty() || r.arch == arch_filter)?;
        let mut missing: Vec<_> = views
            .iter()
            .filter(|v| v.state == BuildState::Missing)
            .cloned()
            .collect();
        let mut outdated: Vec<_> = views
            .iter()
            .filter(|v| v.state == BuildState::Outdated)
            .cloned()
            .collect();
        let ahead: Vec<_> = views
            .iter()
            .filter(|v| v.state == BuildState::Ahead)
            .cloned()
            .collect();
        let built = views
            .iter()
            .filter(|v| v.state == BuildState::Built)
            .count();
        missing.sort_by(|a, b| a.name.cmp(&b.name));
        outdated.sort_by(|a, b| a.name.cmp(&b.name));
        let tpl = StatusTemplate {
            missing,
            outdated,
            ahead,
            built,
            arch: arch_filter,
            arches,
            nav: Nav::from_config(&state.config),
        };
        Ok(Html(tpl.render()?).into_response())
    })
}

/// The canonical recipes tree as the About page presents it.
#[derive(Serialize)]
pub struct SourceInfo {
    pub url: String,
    /// HEAD at the last sync, short form; empty before the first sync.
    pub commit: String,
}

/// `/about` — the site's single recipe source and serving build.
/// which build of recipedia is serving it.
#[derive(Template)]
#[template(path = "about.html")]
pub struct AboutTemplate {
    pub version: String,
    pub source: SourceInfo,
    pub recipe_count: i64,
    pub published_count: i64,
    pub last_sync: Option<SyncEntry>,
    pub nav: Nav,
}

async fn about_page(State(state): State<SharedState>) -> Response {
    with_conn(&state, |conn| {
        let commit = db::meta_get(conn, "last_commit")
            .ok()
            .flatten()
            .map(|full| full[..12.min(full.len())].to_string())
            .unwrap_or_default();
        let tpl = AboutTemplate {
            version: env!("CARGO_PKG_VERSION").to_string(),
            source: SourceInfo {
                url: state.config.git_url.clone(),
                commit,
            },
            recipe_count: db::categories(conn)?.iter().map(|c| c.count).sum(),
            published_count: db::published_all(conn)?.len() as i64,
            last_sync: db::recent_syncs(conn, 1)?.into_iter().next(),
            nav: Nav::from_config(&state.config),
        };
        Ok(Html(tpl.render()?).into_response())
    })
}

#[derive(Template)]
#[template(path = "upload.html")]
pub struct UploadTemplate {
    pub nav: Nav,
}

async fn upload_page(State(state): State<SharedState>) -> Response {
    let tpl = UploadTemplate {
        nav: Nav::from_config(&state.config),
    };
    Html(tpl.render().expect("static template")).into_response()
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
        return Err(Box::new(
            (StatusCode::UNAUTHORIZED, "missing bearer token").into_response(),
        ));
    };
    match db::token_label(&state.db.lock().expect("db mutex poisoned"), token) {
        Ok(Some(label)) => Ok(label),
        Ok(None) => Err(Box::new(
            (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
        )),
        Err(e) => Err(Box::new(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("token check failed: {e:#}"),
            )
                .into_response(),
        )),
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
    let tmp_path =
        state
            .config
            .repo_dir
            .join(format!(".incoming-{}-{}", std::process::id(), filename));
    let mut file = match tokio::fs::File::create(&tmp_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot stage upload: {e}"),
            )
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
        repo::ingest(
            &conn,
            &config,
            &staged,
            &ingest_name,
            &sha256,
            declared.as_deref(),
            &builder,
        )
    })
    .await;
    match result {
        Ok(Ok(receipt)) => json_response(&receipt),
        Ok(Err(e)) => {
            tokio::fs::remove_file(&tmp_path).await.ok();
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("publish rejected: {e:#}"),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("publish task panicked: {e}"),
        )
            .into_response(),
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
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("{filename} is not published"),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("database error: {e:#}"),
            )
                .into_response()
        }
    }
    let config = state.config.clone();
    let target = filename.clone();
    let result = tokio::task::spawn_blocking(move || {
        let conn = state
            .db
            .lock()
            .expect("unpublish worker: db mutex poisoned");
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task panicked: {e}"),
        )
            .into_response(),
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
        Err(_) => {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "log must be UTF-8 text").into_response()
        }
    };
    let target = filename.clone();
    let result: Result<usize> = (|| {
        let conn = state.db.lock().expect("db mutex poisoned");
        // Only logs for things actually published: no orphans, no squatting.
        match db::published_sha256(&conn, &target)? {
            Some(_) => {
                db::log_upsert(&conn, &target, content.trim_end(), &builder).map(|_| body.len())
            }
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
#[derive(Template)]
#[template(path = "repo_index.html")]
pub struct RepoIndexTemplate {
    pub files: Vec<PublishedRow>,
    pub total_mib: String,
    pub nav: Nav,
}

/// Browsable listing of everything published: this is what
/// `https://repo.<host>/` renders (and `/repo/` on the main site).
async fn repo_index(State(state): State<SharedState>) -> Response {
    with_conn(&state, |conn| {
        let files = db::published_all(conn)?;
        let bytes: i64 = files.iter().map(|f| f.size).sum();
        let tpl = RepoIndexTemplate {
            total_mib: format!("{:.1}", bytes as f64 / 1_048_576.0),
            files,
            nav: Nav::from_config(&state.config),
        };
        Ok(Html(tpl.render()?).into_response())
    })
}

async fn repo_file(
    State(state): State<SharedState>,
    AxPath(path): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    if path
        .split('/')
        .any(|seg| seg.is_empty() || seg == ".." || seg == ".")
    {
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
            return (StatusCode::NOT_MODIFIED, [("etag", format!("\"{sha}\""))]).into_response();
        }
    }

    let file = match tokio::fs::File::open(&full).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot open {}: {e}", path),
            )
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
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// JSON API
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ApiStatusEntry {
    name: String,
    arch: String,
    recipe_version: String,
    recipe_release: String,
    repo_version: Option<String>,
    repo_release: Option<String>,
    state: BuildState,
}

/// Short HEAD of the canonical recipes tree for the index page.
fn commits_summary(conn: &Connection) -> Result<String> {
    Ok(db::meta_get(conn, "last_commit")?
        .map(|v| format!("recipes@{}", &v[..12.min(v.len())]))
        .unwrap_or_default())
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
        let published_by_name = published_by_name(conn)?;
        let entry = {
            let published = pick_published(
                published_by_name.get(&name).map(Vec::as_slice),
                &pkg.arch,
                None,
            );
            ApiStatusEntry {
                name: pkg.name.clone(),
                arch: pkg.arch.clone(),
                state: derive(
                    &pkg.version,
                    &pkg.release,
                    published.map(|p| (p.version.as_str(), p.release.as_str())),
                ),
                recipe_version: pkg.version.clone(),
                recipe_release: pkg.release.clone(),
                repo_version: published.map(|p| p.version.clone()),
                repo_release: published.map(|p| p.release.clone()),
            }
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
        let published = published_by_name(conn)?;
        let entries: Vec<ApiStatusEntry> = db::all_packages(conn)?
            .iter()
            .map(|r| {
                // Pair each recipe row with a build of a matching arch and
                // the SAME version when one exists; comparing an old version
                // row against a newer build reported every superseded
                // version as forever 'ahead'.
                let published = pick_published(
                    published.get(&r.name).map(Vec::as_slice),
                    &r.arch,
                    Some(&r.version),
                );
                ApiStatusEntry {
                    name: r.name.clone(),
                    arch: r.arch.clone(),
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
        let conn = worker_state
            .db
            .lock()
            .expect("sync worker: db mutex poisoned");
        sync::run_sync(&conn, &config, trigger)
    })
    .await;
    state.syncing.store(false, Ordering::SeqCst);
    match result {
        Ok(Ok(report)) => (StatusCode::OK, format!("synced: {}", report.summary())).into_response(),
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

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::model::{Dep, Recipe};

pub const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS packages (
  name            TEXT PRIMARY KEY,
  category        TEXT NOT NULL,
  version         TEXT NOT NULL,
  release         TEXT NOT NULL,
  description     TEXT NOT NULL,
  license         TEXT NOT NULL,
  channel         TEXT NOT NULL,
  provides        TEXT NOT NULL,   -- JSON array of strings
  dependencies    TEXT NOT NULL,   -- JSON array of {name, req}
  build_deps      TEXT NOT NULL,   -- JSON array of {name, req}
  conffiles       TEXT NOT NULL,   -- JSON array of strings
  source_url      TEXT NOT NULL,
  source_sha256   TEXT NOT NULL,
  recipe_path     TEXT NOT NULL,   -- repo-relative, links to GitHub raw
  git_commit      TEXT NOT NULL,
  synced_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS published (
  filename     TEXT PRIMARY KEY,   -- <name>-<version>-<release>-<arch>.pkg.tar.zst
  name         TEXT NOT NULL,
  version      TEXT NOT NULL,
  release      TEXT NOT NULL,
  arch         TEXT NOT NULL,
  size         INTEGER NOT NULL,
  sha256       TEXT NOT NULL,
  builder      TEXT NOT NULL,
  uploaded_at  INTEGER NOT NULL,
  meta         TEXT NOT NULL DEFAULT ''  -- JSON ManifestMeta extracted from the archive
);
CREATE INDEX IF NOT EXISTS idx_published_name ON published(name);

CREATE TABLE IF NOT EXISTS tokens (
  id            INTEGER PRIMARY KEY,
  token_hash    TEXT NOT NULL UNIQUE,
  label         TEXT NOT NULL,
  created_at    INTEGER NOT NULL,
  last_used_at  INTEGER
);

CREATE TABLE IF NOT EXISTS sync_log (
  id          INTEGER PRIMARY KEY,
  kind        TEXT NOT NULL,      -- webhook | poll | boot
  sha         TEXT NOT NULL,
  started_at  INTEGER NOT NULL,
  finished_at INTEGER NOT NULL,
  ok          INTEGER NOT NULL,
  message     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS published_files (
  filename TEXT NOT NULL,
  path     TEXT NOT NULL,
  type     TEXT NOT NULL,
  size     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_published_files ON published_files(filename);

CREATE TABLE IF NOT EXISTS build_logs (
  filename    TEXT PRIMARY KEY,
  content     TEXT NOT NULL,
  builder     TEXT NOT NULL,
  uploaded_at INTEGER NOT NULL
);
"#;

pub fn open(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("cannot create database directory {}", parent.display())
        })?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("cannot open database at {}", db_path.display()))?;
    conn.execute_batch(SCHEMA)?;
    // Databases created before P3 lack the published.meta column.
    let _ = conn.execute("ALTER TABLE published ADD COLUMN meta TEXT NOT NULL DEFAULT ''", []);
    // Databases created before multi-arch support have a packages table
    // without the arch/origin columns. It is a disposable cache: drop it and
    // let the next sync rebuild the new shape.
    let has_arch: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'arch'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_arch {
        let _ = conn.execute("DROP TABLE IF EXISTS packages", []);
    }
    Ok(conn)
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageRow {
    pub name: String,
    /// Effective architecture: the declared `arch` canonicalized, falling
    /// back to the tree's architecture when undeclared.
    pub arch: String,
    /// URL of the recipes tree this row was parsed from (GitHub links).
    pub origin: String,
    pub category: String,
    pub version: String,
    pub release: String,
    pub description: String,
    pub license: String,
    pub channel: String,
    pub provides: Vec<String>,
    pub dependencies: Vec<Dep>,
    pub build_dependencies: Vec<Dep>,
    pub conffiles: Vec<String>,
    pub source_url: String,
    pub source_sha256: String,
    pub recipe_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishedRow {
    pub filename: String,
    pub name: String,
    pub version: String,
    pub release: String,
    pub arch: String,
    pub size: i64,
    pub sha256: String,
    /// Who published this build (token label from the upload).
    pub builder: String,
    pub uploaded_at: i64,
    /// Manifest meta stored at publish time; carries the build-provenance
    /// stamps (compiler, flags) when the builder recorded any.
    pub meta: Option<crate::repo::ManifestMeta>,
}

/// Row mapper shared by every `published` query: eight plain columns plus
/// the meta JSON column, deserialized leniently (bad JSON → None).
fn row_to_published(r: &rusqlite::Row<'_>) -> rusqlite::Result<PublishedRow> {
    let meta_json: String = r.get(8)?;
    Ok(PublishedRow {
        filename: r.get(0)?,
        name: r.get(1)?,
        version: r.get(2)?,
        release: r.get(3)?,
        arch: r.get(4)?,
        size: r.get(5)?,
        sha256: r.get(6)?,
        builder: r.get(9)?,
        uploaded_at: r.get(7)?,
        meta: if meta_json.is_empty() {
            None
        } else {
            serde_json::from_str(&meta_json).ok()
        },
    })
}

/// Replace the whole recipe cache in one shot: fill a temporary table, then
/// swap it under the final name inside one transaction. Readers on WAL see
/// either the old or the new world, never a half-sync.
pub fn rebuild_packages(conn: &Connection, recipes: &[RecipeRecord]) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    conn.execute("DROP TABLE IF EXISTS packages_new", [])?;
    // Explicit shape: CREATE TABLE AS SELECT would copy an old table's
    // columns verbatim and silently drop the arch/origin dimensions.
    conn.execute_batch(
        "CREATE TABLE packages_new (
             name            TEXT NOT NULL,
             arch            TEXT NOT NULL,
             origin          TEXT NOT NULL,
             category        TEXT NOT NULL,
             version         TEXT NOT NULL,
             release         TEXT NOT NULL,
             description     TEXT NOT NULL,
             license         TEXT NOT NULL,
             channel         TEXT NOT NULL,
             provides        TEXT NOT NULL,
             dependencies    TEXT NOT NULL,
             build_deps      TEXT NOT NULL,
             conffiles       TEXT NOT NULL,
             source_url      TEXT NOT NULL,
             source_sha256   TEXT NOT NULL,
             recipe_path     TEXT NOT NULL,
             git_commit      TEXT NOT NULL,
             synced_at       INTEGER NOT NULL
         );",
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO packages_new (name, arch, origin, category, version, release, description, license,
             channel, provides, dependencies, build_deps, conffiles, source_url, source_sha256,
             recipe_path, git_commit, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
    )?;
    for r in recipes {
        stmt.execute(params![
            r.recipe.name,
            r.arch,
            r.origin,
            r.category,
            r.recipe.version,
            r.recipe.release,
            r.recipe.description,
            r.recipe.license,
            r.recipe.channel,
            serde_json::to_string(&r.recipe.provides)?,
            serde_json::to_string(&r.recipe.dependencies)?,
            serde_json::to_string(&r.recipe.build_dependencies)?,
            serde_json::to_string(&r.recipe.conffiles)?,
            r.recipe.source_url,
            r.recipe.source_sha256,
            r.recipe_path,
            r.git_commit,
            now,
        ])?;
    }
    conn.execute_batch(
        "BEGIN;
         DROP TABLE IF EXISTS packages;
         ALTER TABLE packages_new RENAME TO packages;
         COMMIT;",
    )?;
    Ok(())
}

/// One walked recipe: the parsed model plus where it sits in the tree.
#[derive(Debug, Clone)]
pub struct RecipeRecord {
    pub recipe: Recipe,
    /// Effective architecture (declared arch, else the tree's).
    pub arch: String,
    /// URL of the recipes tree this record came from.
    pub origin: String,
    pub category: String,
    pub recipe_path: String,
    /// HEAD of the source tree at collection time.
    pub git_commit: String,
}

const PACKAGE_COLS: &str =
    "name, arch, origin, category, version, release, description, license, channel,
     provides, dependencies, build_deps, conffiles, source_url, source_sha256, recipe_path";

fn select_packages(conn: &Connection, where_clause: &str, name: Option<&str>) -> Result<Vec<PackageRow>> {
    let sql = format!("SELECT {PACKAGE_COLS} FROM packages {where_clause}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = match name {
        Some(n) => stmt.query_map(params![n], map_package_row)?,
        None => stmt.query_map([], map_package_row)?,
    }
    .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every recipe version of every package, including superseded ones.
pub fn all_packages(conn: &Connection) -> Result<Vec<PackageRow>> {
    select_packages(conn, "ORDER BY name", None)
}

/// One row per package: its newest recipe version. Lists, the status board
/// and the dependency graph all speak this dialect; detail pages can still
/// descend into [`package_versions`].
pub fn latest_packages(conn: &Connection) -> Result<Vec<PackageRow>> {
    let mut rows = all_packages(conn)?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.arch.cmp(&b.arch))
    });
    rows.dedup_by(|a, b| {
        if a.name == b.name && a.arch == b.arch {
            // `a` precedes `b`; keep whichever sorts newer under the same
            // segment ordering the build-state diff uses.
            if status_cmp(a, b) == std::cmp::Ordering::Greater {
                std::mem::swap(a, b);
            }
            true
        } else {
            false
        }
    });
    Ok(rows)
}

fn status_cmp(a: &PackageRow, b: &PackageRow) -> std::cmp::Ordering {
    crate::status::compare_versions(
        &format!("{}-{}", a.version, a.release),
        &format!("{}-{}", b.version, b.release),
    )
}

/// All recipe versions kept for one package, newest first.
pub fn package_versions(conn: &Connection, name: &str) -> Result<Vec<PackageRow>> {
    let mut rows = select_packages(conn, "WHERE name = ?1", Some(name))?;
    rows.sort_by(|a, b| status_cmp(b, a));
    Ok(rows)
}

/// Newest recipe version of one package, if the name exists at all.
pub fn package_by_name(conn: &Connection, name: &str) -> Result<Option<PackageRow>> {
    Ok(package_versions(conn, name)?.into_iter().next())
}

/// Packages (deduplicated) whose `provides` covers `virtual_name` -- the
/// resolution for names like `virtual/libc` that have no recipe of their own.
pub fn providers(conn: &Connection, virtual_name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT name FROM packages
         WHERE EXISTS (SELECT 1 FROM json_each(packages.provides) j WHERE j.value = ?1)",
    )?;
    let rows = stmt
        .query_map(params![virtual_name], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Packages whose runtime dependencies request `name` (exact dep-name match;
/// version constraints are ignored for the reverse edge).
pub fn reverse_deps(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT p.name FROM packages p, json_each(p.dependencies) j
         WHERE json_extract(j.value, '$.name') = ?1
         ORDER BY p.name",
    )?;
    let rows = stmt
        .query_map(params![name], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Serialize)]
pub struct CategoryCount {
    pub name: String,
    pub count: i64,
}

pub fn categories(conn: &Connection) -> Result<Vec<CategoryCount>> {
    // Distinct names: superseded recipe versions must not inflate counts.
    let mut stmt = conn.prepare(
        "SELECT category, COUNT(DISTINCT name) FROM packages GROUP BY category ORDER BY category",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(CategoryCount {
                name: r.get(0)?,
                count: r.get(1)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Latest published row per (package name, canonical architecture). An old
/// build re-uploaded later must not shadow the current one: latest by
/// *version*, ties broken by upload time. Legacy `x86_64` artifacts fold
/// into `amd64` here, so both spellings meet in one status slot.
pub fn published_latest_by_arch(conn: &Connection) -> Result<Vec<PublishedRow>> {
    let mut stmt = conn.prepare(
        "SELECT filename, name, version, release, arch, size, sha256, uploaded_at, meta, builder FROM published",
    )?;
    let all = stmt
        .query_map([], row_to_published)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut best: std::collections::HashMap<(String, String), PublishedRow> =
        std::collections::HashMap::new();
    for mut row in all {
        row.arch = crate::model::canonical_arch(&row.arch).to_string();
        let key = (row.name.clone(), row.arch.clone());
        match best.get_mut(&key) {
            Some(cur) => {
                let newer = crate::status::compare_versions(
                    &format!("{}-{}", row.version, row.release),
                    &format!("{}-{}", cur.version, cur.release),
                );
                if newer == std::cmp::Ordering::Greater
                    || (newer == std::cmp::Ordering::Equal && row.uploaded_at > cur.uploaded_at)
                {
                    *cur = row;
                }
            }
            None => {
                best.insert(key, row);
            }
        }
    }
    let mut rows: Vec<PublishedRow> = best.into_values().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

/// Every published archive, newest upload first -- the repo file browser.
pub fn published_all(conn: &Connection) -> Result<Vec<PublishedRow>> {
    let mut stmt = conn.prepare(
        "SELECT filename, name, version, release, arch, size, sha256, uploaded_at, meta, builder
         FROM published ORDER BY uploaded_at DESC",
    )?;
    let rows = stmt
        .query_map([], row_to_published)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Every published build of one package, newest first -- the detail page's
/// version ladder of what actually landed in the repository.
pub fn published_for_name(conn: &Connection, name: &str) -> Result<Vec<PublishedRow>> {
    let mut stmt = conn.prepare(
        "SELECT filename, name, version, release, arch, size, sha256, uploaded_at, meta, builder
         FROM published WHERE name = ?1 ORDER BY uploaded_at DESC",
    )?;
    let rows = stmt
        .query_map(params![name], row_to_published)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Stored manifest meta for one archive (empty JSON → None).
pub fn published_meta(conn: &Connection, filename: &str) -> Result<Option<crate::repo::ManifestMeta>> {
    let mut stmt = conn.prepare("SELECT meta FROM published WHERE filename = ?1")?;
    let mut rows = stmt.query_map(params![filename], |r| r.get::<_, String>(0))?;
    match rows.next().transpose()? {
        Some(json) if !json.is_empty() => Ok(serde_json::from_str(&json).ok()),
        _ => Ok(None),
    }
}

/// Everything the index generator needs, including the JSON manifest meta.
pub struct IndexRow {
    pub filename: String,
    pub name: String,
    pub version: String,
    pub release: String,
    pub arch: String,
    pub size: i64,
    pub meta_json: String,
}

pub fn index_rows(conn: &Connection) -> Result<Vec<IndexRow>> {
    let mut stmt = conn.prepare(
        "SELECT filename, name, version, release, arch, size, meta
         FROM published ORDER BY name, filename",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(IndexRow {
                filename: r.get(0)?,
                name: r.get(1)?,
                version: r.get(2)?,
                release: r.get(3)?,
                arch: r.get(4)?,
                size: r.get(5)?,
                meta_json: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Insert or overwrite one published file. Re-uploading the same filename
/// replaces the row (and the file on disk) — publish stays idempotent.
pub fn upsert_published(
    conn: &Connection,
    filename: &str,
    meta: &crate::repo::ManifestMeta,
    size: i64,
    sha256: &str,
    builder: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO published (filename, name, version, release, arch, size, sha256,
             builder, uploaded_at, meta)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(filename) DO UPDATE SET
             name = excluded.name, version = excluded.version, release = excluded.release,
             arch = excluded.arch, size = excluded.size, sha256 = excluded.sha256,
             builder = excluded.builder, uploaded_at = excluded.uploaded_at,
             meta = excluded.meta",
        params![
            filename,
            meta.name,
            meta.version,
            meta.release,
            meta.arch,
            size,
            sha256,
            builder,
            now(),
            serde_json::to_string(meta).map_err(json_err)?,
        ],
    )?;
    Ok(())
}

pub fn published_sha256(conn: &Connection, filename: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT sha256 FROM published WHERE filename = ?1")?;
    let mut rows = stmt.query_map(params![filename], |r| r.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

// ---------------------------------------------------------------------------
// Publish tokens (Bearer auth for /api/repo/publish)
// ---------------------------------------------------------------------------

/// Mint a fresh token: returned once in full, stored only as SHA-256.
pub fn token_create(conn: &Connection, label: &str) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut raw = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    let token = format!("rt_{}", hex::encode(raw));
    let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
    conn.execute(
        "INSERT INTO tokens (token_hash, label, created_at) VALUES (?1, ?2, ?3)",
        params![hash, label, now()],
    )?;
    Ok(token)
}

/// Resolve a presented Bearer token to its label, refreshing last_used_at.
pub fn token_label(conn: &Connection, presented: &str) -> Result<Option<String>> {
    use sha2::Digest;
    let hash = hex::encode(sha2::Sha256::digest(presented.as_bytes()));
    let mut stmt = conn.prepare("SELECT label FROM tokens WHERE token_hash = ?1")?;
    let mut rows = stmt.query_map(params![hash], |r| r.get::<_, String>(0))?;
    let label = match rows.next() {
        Some(row) => Some(row?),
        None => return Ok(None),
    };
    conn.execute(
        "UPDATE tokens SET last_used_at = ?1 WHERE token_hash = ?2",
        params![now(), hash],
    )?;
    Ok(label)
}

// ---------------------------------------------------------------------------
// Published file lists & build logs (P5)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct FileLine {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub size: i64,
}

/// Swap the stored file list for one archive; called on every publish.
pub fn replace_published_files(conn: &Connection, filename: &str, lines: &[FileLine]) -> Result<()> {
    conn.execute("DELETE FROM published_files WHERE filename = ?1", params![filename])?;
    let mut stmt = conn.prepare(
        "INSERT INTO published_files (filename, path, type, size) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for l in lines {
        stmt.execute(params![filename, l.path, l.kind, l.size])?;
    }
    Ok(())
}

pub fn file_list(conn: &Connection, filename: &str) -> Result<Vec<FileLine>> {
    let mut stmt = conn.prepare(
        "SELECT path, type, size FROM published_files
         WHERE filename = ?1 ORDER BY path",
    )?;
    let rows = stmt
        .query_map(params![filename], |r| {
            Ok(FileLine { path: r.get(0)?, kind: r.get(1)?, size: r.get(2)? })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn log_upsert(conn: &Connection, filename: &str, content: &str, builder: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO build_logs (filename, content, builder, uploaded_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(filename) DO UPDATE SET
             content = excluded.content, builder = excluded.builder,
             uploaded_at = excluded.uploaded_at",
        params![filename, content, builder, now()],
    )?;
    Ok(())
}

pub struct LogRow {
    pub content: String,
    pub builder: String,
    pub uploaded_at: i64,
}

pub fn log_get(conn: &Connection, filename: &str) -> Result<Option<LogRow>> {
    let mut stmt =
        conn.prepare("SELECT content, builder, uploaded_at FROM build_logs WHERE filename = ?1")?;
    let mut rows = stmt.query_map(params![filename], |r| {
        Ok(LogRow { content: r.get(0)?, builder: r.get(1)?, uploaded_at: r.get(2)? })
    })?;
    Ok(rows.next().transpose()?)
}

/// Remove a published artifact everywhere: row, file list, build log.
/// Returns false when the filename was never published. The on-disk file and
/// index regeneration are the caller's job (they own repo_dir).
pub fn delete_published(conn: &Connection, filename: &str) -> Result<bool> {
    let affected = conn.execute("DELETE FROM published WHERE filename = ?1", params![filename])?;
    if affected == 0 {
        return Ok(false);
    }
    conn.execute("DELETE FROM published_files WHERE filename = ?1", params![filename])?;
    conn.execute("DELETE FROM build_logs WHERE filename = ?1", params![filename])?;
    Ok(true)
}

pub fn recent_syncs(conn: &Connection, limit: i64) -> Result<Vec<SyncEntry>> {
    let mut stmt = conn.prepare(
        "SELECT kind, sha, started_at, ok, message
         FROM sync_log ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(SyncEntry {
                trigger: r.get(0)?,
                commit: r.get(1)?,
                started_at: r.get(2)?,
                ok: r.get::<_, i64>(3)? != 0,
                message: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[derive(Debug, Serialize)]
pub struct SyncEntry {
    pub trigger: String,
    pub commit: String,
    pub started_at: i64,
    pub ok: bool,
    pub message: String,
}

impl SyncEntry {
    /// Renderable UTC timestamp ("YYYY-MM-DD HH:MM") for templates.
    pub fn when(&self) -> String {
        time_hm_pub(self.started_at)
    }
    /// Same instant as UTC ISO-8601 for `<time datetime>`.
    pub fn when_iso(&self) -> String {
        time_utc(self.started_at)
    }
}

/// One renderable build-flag line of the provenance block.
pub struct FlagLine {
    pub label: &'static str,
    pub value: String,
}

impl PublishedRow {
    /// When this build landed in the repository (template helper).
    pub fn built_at(&self) -> String {
        time_hm_pub(self.uploaded_at)
    }
    /// Same instant as UTC ISO-8601 for `<time datetime>` (client-side
    /// timezone conversion).
    pub fn built_iso(&self) -> String {
        time_utc(self.uploaded_at)
    }
    /// Archive size in MiB, one decimal (template helper).
    pub fn size_mib(&self) -> String {
        format!("{:.1}", self.size as f64 / 1_048_576.0)
    }
    /// True when the builder stamped any build provenance. Packages without
    /// compilation evidence (os-release and friends) stay unstamped and the
    /// page renders nothing rather than an inference.
    pub fn has_build_info(&self) -> bool {
        self.meta.as_ref().is_some_and(|m| {
            !(m.build_compiler.is_empty()
                && m.build_cflags.is_empty()
                && m.build_cxxflags.is_empty()
                && m.build_ldflags.is_empty())
        })
    }
    /// "clang 22.1.8" style stamp; empty when the build recorded no compiler.
    /// Sage's paired form ("clang: 22.1.8, gcc: 15.3.0" — one version per
    /// producer, crt traces included) renders as "clang 22.1.8 · gcc 15.3.0".
    pub fn compiler_line(&self) -> String {
        let Some(m) = &self.meta else {
            return String::new();
        };
        if m.build_compiler.is_empty() {
            return String::new();
        }
        if m.build_compiler_version.is_empty() {
            return m.build_compiler.clone();
        }
        if m.build_compiler_version.contains(": ") {
            m.build_compiler_version
                .split(", ")
                .map(|pair| pair.replace(": ", " "))
                .collect::<Vec<_>>()
                .join(" · ")
        } else {
            format!("{} {}", m.build_compiler, m.build_compiler_version)
        }
    }
    /// The stamped flag lines, in display order; empty when none recorded.
    pub fn flag_lines(&self) -> Vec<FlagLine> {
        let Some(m) = &self.meta else {
            return Vec::new();
        };
        [
            ("CFLAGS", &m.build_cflags),
            ("CXXFLAGS", &m.build_cxxflags),
            ("LDFLAGS", &m.build_ldflags),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(label, value)| FlagLine {
            label,
            value: value.clone(),
        })
        .collect()
    }
}

/// Unix seconds → `YYYY-MM-DD HH:MM:SSZ` UTC via civil-from-days
/// (no chrono dependency); the single source of wall-clock rendering.
pub fn time_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// "YYYY-MM-DD HH:MM" — the human-facing slice of [`time_utc`].
pub fn time_hm_pub(unix: i64) -> String {
    time_utc(unix).replacen('T', " ", 1)[..16].to_string()
}

pub fn log_sync(
    conn: &Connection,
    trigger: &str,
    commit: &str,
    started_at: i64,
    ok: bool,
    message: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_log (kind, sha, started_at, finished_at, ok, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![trigger, commit, started_at, now(), ok as i64, message],
    )?;
    Ok(())
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = stmt.query_map(params![key], |r| r.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn json_err(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
}

fn map_package_row(
    r: &rusqlite::Row<'_>,
) -> std::result::Result<PackageRow, rusqlite::Error> {
    Ok(PackageRow {
        name: r.get(0)?,
        arch: r.get(1)?,
        origin: r.get(2)?,
        category: r.get(3)?,
        version: r.get(4)?,
        release: r.get(5)?,
        description: r.get(6)?,
        license: r.get(7)?,
        channel: r.get(8)?,
        provides: serde_json::from_str(&r.get::<_, String>(9)?).map_err(json_err)?,
        dependencies: serde_json::from_str(&r.get::<_, String>(10)?).map_err(json_err)?,
        build_dependencies: serde_json::from_str(&r.get::<_, String>(11)?).map_err(json_err)?,
        conffiles: serde_json::from_str(&r.get::<_, String>(12)?).map_err(json_err)?,
        source_url: r.get(13)?,
        source_sha256: r.get(14)?,
        recipe_path: r.get(15)?,
    })
}

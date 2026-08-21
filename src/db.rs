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
  uploaded_at  INTEGER NOT NULL
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
    Ok(conn)
}

#[derive(Debug, Serialize)]
pub struct PackageRow {
    pub name: String,
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
    pub uploaded_at: i64,
}

/// Replace the whole recipe cache in one shot: fill a temporary table, then
/// swap it under the final name inside one transaction. Readers on WAL see
/// either the old or the new world, never a half-sync.
pub fn rebuild_packages(
    conn: &Connection,
    recipes: &[RecipeRecord],
    git_commit: &str,
) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    conn.execute("DROP TABLE IF EXISTS packages_new", [])?;
    conn.execute_batch(
        "CREATE TABLE packages_new AS SELECT * FROM packages WHERE 0;",
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO packages_new (name, category, version, release, description, license,
             channel, provides, dependencies, build_deps, conffiles, source_url, source_sha256,
             recipe_path, git_commit, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
    )?;
    for r in recipes {
        stmt.execute(params![
            r.recipe.name,
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
            git_commit,
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
    pub category: String,
    pub recipe_path: String,
}

pub fn all_packages(conn: &Connection) -> Result<Vec<PackageRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, category, version, release, description, license, channel,
                provides, dependencies, build_deps, conffiles, source_url, source_sha256,
                recipe_path
         FROM packages ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], map_package_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn package_by_name(conn: &Connection, name: &str) -> Result<Option<PackageRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, category, version, release, description, license, channel,
                provides, dependencies, build_deps, conffiles, source_url, source_sha256,
                recipe_path
         FROM packages WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(params![name], map_package_row)?;
    Ok(rows.next().transpose()?)
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
    let mut stmt = conn.prepare(
        "SELECT category, COUNT(*) FROM packages GROUP BY category ORDER BY category",
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

/// Latest published row per package name (idempotent re-uploads keep one row
/// per filename, but a name may have several filenames across rebuilds).
pub fn published_latest_by_name(conn: &Connection) -> Result<Vec<PublishedRow>> {
    let mut stmt = conn.prepare(
        "SELECT filename, name, version, release, arch, size, sha256, uploaded_at
         FROM published p
         WHERE uploaded_at = (SELECT MAX(uploaded_at) FROM published q WHERE q.name = p.name)",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PublishedRow {
                filename: r.get(0)?,
                name: r.get(1)?,
                version: r.get(2)?,
                release: r.get(3)?,
                arch: r.get(4)?,
                size: r.get(5)?,
                sha256: r.get(6)?,
                uploaded_at: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
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
        category: r.get(1)?,
        version: r.get(2)?,
        release: r.get(3)?,
        description: r.get(4)?,
        license: r.get(5)?,
        channel: r.get(6)?,
        provides: serde_json::from_str(&r.get::<_, String>(7)?).map_err(json_err)?,
        dependencies: serde_json::from_str(&r.get::<_, String>(8)?).map_err(json_err)?,
        build_dependencies: serde_json::from_str(&r.get::<_, String>(9)?).map_err(json_err)?,
        conffiles: serde_json::from_str(&r.get::<_, String>(10)?).map_err(json_err)?,
        source_url: r.get(11)?,
        source_sha256: r.get(12)?,
        recipe_path: r.get(13)?,
    })
}

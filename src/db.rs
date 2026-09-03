//! Recipedia's persistent state, backed entirely by LMDB.
//!
//! Sage owns the package and repository wire formats. This database stores
//! the hub's application state (recipe observations, publish receipts,
//! credentials, logs, and sync metadata) in named LMDB tables; no SQLite
//! compatibility layer or legacy schema is retained.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use heed::types::{Bytes, Str};
use heed::{Database as HeedDatabase, Env, EnvOpenOptions};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::model::{canonical_arch, Dep};
use crate::repo::ManifestMeta;

const MAP_SIZE: usize = 8 * 1024 * 1024 * 1024;
static SYNC_ID: AtomicU64 = AtomicU64::new(0);

/// Named LMDB tables used by the hub.
pub struct Database {
    env: Env,
    recipes: HeedDatabase<Str, Bytes>,
    published: HeedDatabase<Str, Bytes>,
    files: HeedDatabase<Str, Bytes>,
    logs: HeedDatabase<Str, Bytes>,
    tokens: HeedDatabase<Str, Bytes>,
    meta: HeedDatabase<Str, Str>,
    sync_log: HeedDatabase<Str, Bytes>,
    db_path: std::path::PathBuf,
}

/// Open or create the current LMDB environment and all schema-v1 tables.
pub fn open(db_path: &Path) -> Result<Database> {
    if db_path.exists() && !db_path.is_dir() {
        anyhow::bail!(
            "database path {} is a file; Sage 0.4 state requires an LMDB directory",
            db_path.display()
        );
    }
    std::fs::create_dir_all(db_path)
        .with_context(|| format!("cannot create LMDB directory {}", db_path.display()))?;
    // SAFETY: this process is the sole writer of the configured hub state and
    // keeps one fixed map size for every opener.
    let env = unsafe {
        EnvOpenOptions::new()
            .map_size(MAP_SIZE)
            .max_dbs(16)
            .open(db_path)?
    };
    let mut txn = env.write_txn()?;
    let recipes = env.create_database(&mut txn, Some("recipes"))?;
    let published = env.create_database(&mut txn, Some("published"))?;
    let files = env.create_database(&mut txn, Some("files"))?;
    let logs = env.create_database(&mut txn, Some("logs"))?;
    let tokens = env.create_database(&mut txn, Some("tokens"))?;
    let meta = env.create_database(&mut txn, Some("meta"))?;
    let sync_log = env.create_database(&mut txn, Some("sync_log"))?;
    txn.commit()?;
    Ok(Database {
        env,
        recipes,
        published,
        files,
        logs,
        tokens,
        meta,
        sync_log,
        db_path: db_path.to_path_buf(),
    })
}

impl Database {
    pub fn path(&self) -> &Path {
        &self.db_path
    }
}

fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    Ok(bincode::serialize(value)?)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(bincode::deserialize(bytes)?)
}

fn read_all<T: DeserializeOwned>(env: &Env, database: &HeedDatabase<Str, Bytes>) -> Result<Vec<T>> {
    let txn = env.read_txn()?;
    let mut values = Vec::new();
    for item in database.iter(&txn)? {
        let (_, bytes) = item?;
        values.push(decode(bytes)?);
    }
    Ok(values)
}

fn read_one<T: DeserializeOwned>(
    env: &Env,
    database: &HeedDatabase<Str, Bytes>,
    key: &str,
) -> Result<Option<T>> {
    let txn = env.read_txn()?;
    database.get(&txn, key)?.map(decode).transpose()
}

fn put<T: Serialize + ?Sized>(
    txn: &mut heed::RwTxn<'_>,
    database: &HeedDatabase<Str, Bytes>,
    key: &str,
    value: &T,
) -> Result<()> {
    database.put(txn, key, &encode(value)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Recipe catalog
// ---------------------------------------------------------------------------

/// One Sage recipe observed in the canonical source tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeRecord {
    pub package: sage_core::Package,
    pub origin: String,
    pub category: String,
    pub recipe_path: String,
    pub git_commit: String,
    pub source_url: String,
    pub source_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageRow {
    pub name: String,
    pub slot: String,
    pub epoch: u32,
    pub arch: String,
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
    pub check_dependencies: Vec<Dep>,
    pub conffiles: Vec<String>,
    pub source_url: String,
    pub source_sha256: String,
    pub upstream_url: String,
    pub upstream_version_regex: String,
    pub recipe_path: String,
}

fn recipe_key(record: &RecipeRecord) -> String {
    let package = &record.package;
    format!(
        "{}:{}:{}:{}:{}:{}",
        package.channel,
        package.name,
        package.slot,
        package.arch,
        package.epoch,
        package.coordinate().version
    )
}

fn package_row(record: &RecipeRecord) -> PackageRow {
    let package = &record.package;
    PackageRow {
        name: package.name.clone(),
        slot: package.slot.clone(),
        epoch: package.epoch,
        arch: canonical_arch(&package.arch).to_string(),
        origin: record.origin.clone(),
        category: record.category.clone(),
        version: package.version.clone(),
        release: package.release.to_string(),
        description: package.description.clone(),
        license: package.license.clone(),
        channel: package.channel.clone(),
        provides: package.provides.clone(),
        dependencies: package.dependencies.iter().map(Dep::from).collect(),
        build_dependencies: Vec::new(),
        check_dependencies: Vec::new(),
        conffiles: Vec::new(),
        source_url: record.source_url.clone(),
        source_sha256: record.source_sha256.clone(),
        upstream_url: String::new(),
        upstream_version_regex: String::new(),
        recipe_path: record.recipe_path.clone(),
    }
}

/// Atomically replace the recipe observation set after one Sage parser pass.
pub fn rebuild_packages(database: &Database, recipes: &[RecipeRecord]) -> Result<()> {
    let mut txn = database.env.write_txn()?;
    database.recipes.clear(&mut txn)?;
    for record in recipes {
        put(&mut txn, &database.recipes, &recipe_key(record), record)?;
    }
    txn.commit()?;
    Ok(())
}

pub fn all_packages(database: &Database) -> Result<Vec<PackageRow>> {
    let records = read_all::<RecipeRecord>(&database.env, &database.recipes)?;
    let mut rows: Vec<_> = records.iter().map(package_row).collect();
    rows.sort_by(|a, b| {
        (
            &a.name, &a.arch, &a.channel, &a.slot, &a.version, &a.release,
        )
            .cmp(&(
                &b.name, &b.arch, &b.channel, &b.slot, &b.version, &b.release,
            ))
    });
    Ok(rows)
}

pub fn latest_packages(database: &Database) -> Result<Vec<PackageRow>> {
    let records = read_all::<RecipeRecord>(&database.env, &database.recipes)?;
    let mut latest: BTreeMap<(String, String, String, String), RecipeRecord> = BTreeMap::new();
    for record in records {
        let package = &record.package;
        let key = (
            package.name.clone(),
            canonical_arch(&package.arch).to_string(),
            package.channel.clone(),
            package.slot.clone(),
        );
        let replace = latest.get(&key).is_none_or(|current| {
            package.coordinate().version > current.package.coordinate().version
        });
        if replace {
            latest.insert(key, record);
        }
    }
    let mut rows: Vec<_> = latest.values().map(package_row).collect();
    rows.sort_by(|a, b| {
        (&a.name, &a.arch, &a.channel, &a.slot).cmp(&(&b.name, &b.arch, &b.channel, &b.slot))
    });
    Ok(rows)
}

pub fn package_versions(database: &Database, name: &str) -> Result<Vec<PackageRow>> {
    let records = read_all::<RecipeRecord>(&database.env, &database.recipes)?;
    let mut rows: Vec<_> = records
        .iter()
        .filter(|record| record.package.name == name)
        .map(package_row)
        .collect();
    rows.sort_by(|a, b| {
        crate::status::compare_versions(
            &format!("{}:{}-{}", b.epoch, b.version, b.release),
            &format!("{}:{}-{}", a.epoch, a.version, a.release),
        )
    });
    Ok(rows)
}

pub fn package_by_name(database: &Database, name: &str) -> Result<Option<PackageRow>> {
    Ok(package_versions(database, name)?.into_iter().next())
}

pub fn providers(database: &Database, virtual_name: &str) -> Result<Vec<String>> {
    let mut names = HashSet::new();
    for record in read_all::<RecipeRecord>(&database.env, &database.recipes)? {
        if record
            .package
            .provides
            .iter()
            .any(|value| value == virtual_name)
        {
            names.insert(record.package.name);
        }
    }
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort();
    Ok(names)
}

pub fn reverse_deps(database: &Database, name: &str) -> Result<Vec<String>> {
    let mut names = HashSet::new();
    for record in read_all::<RecipeRecord>(&database.env, &database.recipes)? {
        if record
            .package
            .dependencies
            .iter()
            .any(|dependency| dependency.name == name)
        {
            names.insert(record.package.name);
        }
    }
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort();
    Ok(names)
}

#[derive(Debug, Serialize)]
pub struct CategoryCount {
    pub name: String,
    pub count: i64,
}

pub fn categories(database: &Database) -> Result<Vec<CategoryCount>> {
    let mut grouped: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for row in latest_packages(database)? {
        grouped.entry(row.category).or_default().insert(row.name);
    }
    Ok(grouped
        .into_iter()
        .map(|(name, packages)| CategoryCount {
            name,
            count: packages.len() as i64,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Published artifacts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedRow {
    pub filename: String,
    /// Path below the public `/repo/` root, e.g. `system/foo.pkg.tar.zst`.
    pub repo_path: String,
    pub name: String,
    pub slot: String,
    pub version: String,
    pub release: String,
    pub epoch: u32,
    pub arch: String,
    pub channel: String,
    pub size: i64,
    pub sha256: String,
    pub builder: String,
    pub uploaded_at: i64,
    pub meta: Option<ManifestMeta>,
}

pub fn published_latest_by_arch(database: &Database) -> Result<Vec<PublishedRow>> {
    let mut best: HashMap<(String, String, String, String), PublishedRow> = HashMap::new();
    for mut row in read_all::<PublishedRow>(&database.env, &database.published)? {
        row.arch = canonical_arch(&row.arch).to_string();
        let key = (
            row.channel.clone(),
            row.name.clone(),
            row.slot.clone(),
            row.arch.clone(),
        );
        match best.get_mut(&key) {
            Some(current) => {
                let order = crate::status::compare_versions(
                    &format!("{}:{}-{}", row.epoch, row.version, row.release),
                    &format!("{}:{}-{}", current.epoch, current.version, current.release),
                );
                if order == std::cmp::Ordering::Greater
                    || (order == std::cmp::Ordering::Equal && row.uploaded_at > current.uploaded_at)
                {
                    *current = row;
                }
            }
            None => {
                best.insert(key, row);
            }
        }
    }
    let mut rows: Vec<_> = best.into_values().collect();
    rows.sort_by(|a, b| {
        (&a.name, &a.arch, &a.channel, &a.slot).cmp(&(&b.name, &b.arch, &b.channel, &b.slot))
    });
    Ok(rows)
}

pub fn published_all(database: &Database) -> Result<Vec<PublishedRow>> {
    let mut rows = read_all::<PublishedRow>(&database.env, &database.published)?;
    rows.sort_by(|a, b| {
        b.uploaded_at
            .cmp(&a.uploaded_at)
            .then(a.filename.cmp(&b.filename))
    });
    Ok(rows)
}

pub fn published_for_name(database: &Database, name: &str) -> Result<Vec<PublishedRow>> {
    Ok(published_all(database)?
        .into_iter()
        .filter(|row| row.name == name)
        .collect())
}

pub fn published_meta(database: &Database, filename: &str) -> Result<Option<ManifestMeta>> {
    Ok(
        read_one::<PublishedRow>(&database.env, &database.published, filename)?
            .and_then(|row| row.meta),
    )
}

pub fn published_row(database: &Database, filename: &str) -> Result<Option<PublishedRow>> {
    read_one(&database.env, &database.published, filename)
}

pub fn upsert_published(database: &Database, row: &PublishedRow) -> Result<()> {
    let mut txn = database.env.write_txn()?;
    put(&mut txn, &database.published, &row.filename, row)?;
    txn.commit()?;
    Ok(())
}

pub fn published_sha256(database: &Database, filename: &str) -> Result<Option<String>> {
    Ok(published_row(database, filename)?.map(|row| row.sha256))
}

pub fn published_sha256_for_path(database: &Database, repo_path: &str) -> Result<Option<String>> {
    Ok(published_all(database)?
        .into_iter()
        .find(|row| row.repo_path == repo_path)
        .map(|row| row.sha256))
}

/// Remove one artifact and all hub metadata in one write transaction.
pub fn delete_published(database: &Database, filename: &str) -> Result<Option<PublishedRow>> {
    let mut txn = database.env.write_txn()?;
    let Some(row): Option<PublishedRow> = database
        .published
        .get(&txn, filename)?
        .map(decode)
        .transpose()?
    else {
        return Ok(None);
    };
    database.published.delete(&mut txn, filename)?;
    database.files.delete(&mut txn, filename)?;
    database.logs.delete(&mut txn, filename)?;
    txn.commit()?;
    Ok(Some(row))
}

// ---------------------------------------------------------------------------
// Tokens, file inventories, logs, and sync metadata
// ---------------------------------------------------------------------------

pub fn token_create(database: &Database, label: &str) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut raw = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    let token = format!("rt_{}", hex::encode(raw));
    let hash = hex::encode(sha2::Sha256::digest(token.as_bytes()));
    let mut txn = database.env.write_txn()?;
    put(
        &mut txn,
        &database.tokens,
        &hash,
        &TokenRecord {
            label: label.to_string(),
            created_at: now(),
            last_used_at: None,
        },
    )?;
    txn.commit()?;
    Ok(token)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRecord {
    label: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

pub fn token_label(database: &Database, presented: &str) -> Result<Option<String>> {
    use sha2::Digest;
    let hash = hex::encode(sha2::Sha256::digest(presented.as_bytes()));
    let mut txn = database.env.write_txn()?;
    let Some(mut record): Option<TokenRecord> =
        database.tokens.get(&txn, &hash)?.map(decode).transpose()?
    else {
        return Ok(None);
    };
    record.last_used_at = Some(now());
    put(&mut txn, &database.tokens, &hash, &record)?;
    let label = record.label;
    txn.commit()?;
    Ok(Some(label))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLine {
    pub path: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub size: i64,
}

pub fn replace_published_files(
    database: &Database,
    filename: &str,
    lines: &[FileLine],
) -> Result<()> {
    let mut txn = database.env.write_txn()?;
    put(&mut txn, &database.files, filename, lines)?;
    txn.commit()?;
    Ok(())
}

pub fn file_list(database: &Database, filename: &str) -> Result<Vec<FileLine>> {
    Ok(read_one(&database.env, &database.files, filename)?.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRow {
    pub content: String,
    pub builder: String,
    pub uploaded_at: i64,
}

pub fn log_upsert(database: &Database, filename: &str, content: &str, builder: &str) -> Result<()> {
    let mut txn = database.env.write_txn()?;
    put(
        &mut txn,
        &database.logs,
        filename,
        &LogRow {
            content: content.to_string(),
            builder: builder.to_string(),
            uploaded_at: now(),
        },
    )?;
    txn.commit()?;
    Ok(())
}

pub fn log_get(database: &Database, filename: &str) -> Result<Option<LogRow>> {
    read_one(&database.env, &database.logs, filename)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    pub trigger: String,
    pub commit: String,
    pub started_at: i64,
    pub ok: bool,
    pub message: String,
}

impl SyncEntry {
    pub fn when(&self) -> String {
        time_hm_pub(self.started_at)
    }

    pub fn when_iso(&self) -> String {
        time_utc(self.started_at)
    }
}

pub fn recent_syncs(database: &Database, limit: usize) -> Result<Vec<SyncEntry>> {
    let mut entries = read_all::<SyncEntry>(&database.env, &database.sync_log)?;
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.started_at));
    entries.truncate(limit);
    Ok(entries)
}

pub fn log_sync(
    database: &Database,
    trigger: &str,
    commit: &str,
    started_at: i64,
    ok: bool,
    message: &str,
) -> Result<()> {
    let key = format!(
        "{:020}-{:020}",
        now(),
        SYNC_ID.fetch_add(1, Ordering::Relaxed)
    );
    let mut txn = database.env.write_txn()?;
    put(
        &mut txn,
        &database.sync_log,
        &key,
        &SyncEntry {
            trigger: trigger.to_string(),
            commit: commit.to_string(),
            started_at,
            ok,
            message: message.to_string(),
        },
    )?;
    txn.commit()?;
    Ok(())
}

pub fn meta_get(database: &Database, key: &str) -> Result<Option<String>> {
    let txn = database.env.read_txn()?;
    Ok(database.meta.get(&txn, key)?.map(str::to_string))
}

pub fn meta_set(database: &Database, key: &str, value: &str) -> Result<()> {
    let mut txn = database.env.write_txn()?;
    database.meta.put(&mut txn, key, value)?;
    txn.commit()?;
    Ok(())
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

impl PublishedRow {
    pub fn built_at(&self) -> String {
        time_hm_pub(self.uploaded_at)
    }

    pub fn built_iso(&self) -> String {
        time_utc(self.uploaded_at)
    }

    pub fn size_mib(&self) -> String {
        format!("{:.1}", self.size as f64 / 1_048_576.0)
    }

    pub fn is_daemon(&self) -> bool {
        self.meta
            .as_ref()
            .is_some_and(|meta| !meta.service_toml.is_empty())
    }

    pub fn managed_build_tools(&self) -> &[sage_core::ManagedBuildTool] {
        self.meta
            .as_ref()
            .map(|meta| meta.managed_build_tools.as_slice())
            .unwrap_or_default()
    }
}

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

pub fn time_hm_pub(unix: i64) -> String {
    time_utc(unix).replacen('T', " ", 1)[..16].to_string()
}

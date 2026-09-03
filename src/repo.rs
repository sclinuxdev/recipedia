//! Sage 0.4 network-repository publisher.
//!
//! Archives are inspected by sage-archive, stored beneath their Sage
//! channel, and indexed by sage-repo. The public repository therefore
//! exposes the exact files Sage clients consume: one signed index.mdb and
//! compressed index.mdb.zst per subchannel.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use sha2::Digest;

use crate::config::Config;
use crate::db::{self, PublishedRow};
use crate::status::{self, State as BuildState};

pub use sage_core::ManagedBuildTool;

/// UI metadata derived from Sage's canonical archive manifest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManifestMeta {
    pub schema_version: u32,
    pub name: String,
    pub slot: String,
    pub version: String,
    pub release: String,
    pub epoch: u32,
    pub description: String,
    pub license: String,
    pub channel: String,
    pub arch: String,
    pub installed_size: u64,
    pub service_toml: String,
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub check_dependencies: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub features: Vec<String>,
    pub conffiles: Vec<String>,
    pub managed_build_tools: Vec<ManagedBuildTool>,
}

impl From<(&sage_core::Package, &sage_archive::PackageInspection)> for ManifestMeta {
    fn from(
        (package, inspection): (&sage_core::Package, &sage_archive::PackageInspection),
    ) -> Self {
        Self {
            schema_version: package.schema_version,
            name: package.name.clone(),
            slot: package.slot.clone(),
            version: package.version.clone(),
            release: package.release.to_string(),
            epoch: package.epoch,
            description: package.description.clone(),
            license: package.license.clone(),
            channel: package.channel.clone(),
            arch: crate::model::canonical_arch(&package.arch).to_string(),
            installed_size: package.installed_size,
            service_toml: inspection
                .optional
                .get(".METADATA/service.toml")
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default(),
            dependencies: package
                .dependencies
                .iter()
                .map(ToString::to_string)
                .collect(),
            check_dependencies: Vec::new(),
            provides: package.provides.clone(),
            conflicts: package.conflicts.clone(),
            features: package.features.clone(),
            conffiles: Vec::new(),
            managed_build_tools: package.managed_build_tools.clone(),
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Receipt {
    pub filename: String,
    pub repo_path: String,
    pub name: String,
    pub version: String,
    pub release: String,
    pub size: i64,
    pub sha256: String,
    pub state: BuildState,
}

/// Accept only a basename shaped like a Sage package archive.
pub fn valid_filename(name: &str) -> bool {
    name.len() <= 255
        && name.ends_with(".pkg.tar.zst")
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

pub fn read_manifest_meta(archive: &Path) -> Result<ManifestMeta> {
    let inspection = sage_archive::inspect_package(archive)
        .map_err(|error| anyhow::anyhow!("invalid Sage archive: {error}"))?;
    Ok(ManifestMeta::from((&inspection.manifest, &inspection)))
}

pub fn read_file_list(archive: &Path) -> Result<Vec<db::FileLine>> {
    let inspection = sage_archive::inspect_package(archive)
        .map_err(|error| anyhow::anyhow!("invalid Sage archive: {error}"))?;
    Ok(inspection
        .files
        .into_iter()
        .map(|record| db::FileLine {
            path: record.path.to_string_lossy().into_owned(),
            kind: "file".into(),
            size: record.size as i64,
        })
        .collect())
}

fn valid_channel_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// Normalize a manifest channel to the directory used by Sage's
/// ChannelsConfig: system becomes main/system, while an already qualified
/// main/python3.14 is preserved.
pub fn qualified_channel(config: &Config, channel: &str) -> Result<String> {
    let channel = if channel.trim().is_empty() {
        "system"
    } else {
        channel.trim()
    };
    let qualified = if channel.contains('/') {
        channel.to_string()
    } else {
        format!("{}/{}", config.repo_channel.trim_matches('/'), channel)
    };
    let mut parts = qualified.split('/');
    let root = parts.next().unwrap_or_default();
    let subchannel = parts.next().unwrap_or_default();
    if parts.next().is_some() || !valid_channel_segment(root) || !valid_channel_segment(subchannel)
    {
        bail!("invalid Sage channel '{channel}'")
    }
    Ok(format!("{root}/{subchannel}"))
}

fn channel_dir(config: &Config, channel: &str) -> Result<(String, PathBuf)> {
    let qualified = qualified_channel(config, channel)?;
    let mut parts = qualified.split('/');
    let root = parts.next().expect("qualified channel has a root");
    let subchannel = parts.next().expect("qualified channel has a subchannel");
    // Sage appends a subchannel alias to ChannelConfig.url. The configured
    // root channel is represented by that URL, so its physical files live at
    // repo/<subchannel>, not repo/<root>/<subchannel>.
    let physical = if root == config.repo_channel.trim_matches('/') {
        subchannel.to_string()
    } else {
        qualified
    };
    Ok((physical.clone(), config.repo_dir.join(&physical)))
}

fn signing_key_path(config: &Config) -> &Path {
    &config.repo_signing_key
}

/// Create a durable private key on first use and publish its matching public
/// key next to it. Operators can point channels.toml at the .pub file.
fn ensure_signing_key(config: &Config) -> Result<()> {
    let path = signing_key_path(config);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        let mut raw = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(&raw)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let key = sage_repo::decode_fixed::<32>(&raw)
        .map_err(|error| anyhow::anyhow!("invalid Ed25519 repository key: {error}"))?;
    let public_path = path.with_extension("pub");
    if !public_path.exists() {
        std::fs::write(
            &public_path,
            SigningKey::from_bytes(&key).verifying_key().to_bytes(),
        )?;
    }
    Ok(())
}

/// Rebuild every network-repository subchannel using Sage's own indexer.
pub fn regenerate_index(database: &db::Database, config: &Config) -> Result<()> {
    ensure_signing_key(config)?;
    let mut dirs = BTreeSet::new();
    dirs.insert(channel_dir(config, "system")?.1);
    for row in db::published_all(database)? {
        dirs.insert(
            config.repo_dir.join(
                Path::new(&row.repo_path)
                    .parent()
                    .context("published repo path has no channel")?,
            ),
        );
    }
    collect_index_dirs(&config.repo_dir, &mut dirs)?;
    for dir in dirs {
        std::fs::create_dir_all(&dir)?;
        sage_repo::build_index(&dir, &dir, signing_key_path(config))
            .map_err(|error| anyhow::anyhow!("building {}: {error}", dir.display()))?;
    }
    Ok(())
}

fn collect_index_dirs(root: &Path, dirs: &mut BTreeSet<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if path.join("index.mdb").exists() {
                dirs.insert(path.clone());
            }
            collect_index_dirs(&path, dirs)?;
        }
    }
    Ok(())
}

/// Store one fully written archive and rebuild the affected Sage index.
pub fn ingest(
    database: &db::Database,
    config: &Config,
    tmp_path: &Path,
    filename: &str,
    sha256: &str,
    declared_sha: Option<&str>,
    builder: &str,
) -> Result<Receipt> {
    if !valid_filename(filename) {
        bail!("invalid package filename '{filename}'")
    }
    if declared_sha.is_some_and(|declared| declared != sha256) {
        bail!("sha256 mismatch between client declaration and upload")
    }
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256")
    }
    let inspection = sage_archive::inspect_package(tmp_path)
        .map_err(|error| anyhow::anyhow!("invalid Sage archive: {error}"))?;
    let meta = ManifestMeta::from((&inspection.manifest, &inspection));
    let files: Vec<_> = inspection
        .files
        .iter()
        .map(|record| db::FileLine {
            path: record.path.to_string_lossy().into_owned(),
            kind: "file".into(),
            size: record.size as i64,
        })
        .collect();
    let size = std::fs::metadata(tmp_path)?.len() as i64;
    let (qualified, directory) = channel_dir(config, &meta.channel)?;
    std::fs::create_dir_all(&directory)?;
    let repo_path = format!("{qualified}/{filename}");
    if let Some(previous) = db::published_row(database, filename)? {
        if previous.repo_path != repo_path {
            remove_if_file(&config.repo_dir.join(&previous.repo_path))?;
        }
    }
    let destination = directory.join(filename);
    std::fs::rename(tmp_path, &destination)
        .with_context(|| format!("moving upload into {}", destination.display()))?;
    let row = PublishedRow {
        filename: filename.to_string(),
        repo_path,
        name: meta.name.clone(),
        slot: meta.slot.clone(),
        version: meta.version.clone(),
        release: meta.release.clone(),
        epoch: meta.epoch,
        arch: meta.arch.clone(),
        channel: meta.channel.clone(),
        size,
        sha256: sha256.to_ascii_lowercase(),
        builder: builder.to_string(),
        uploaded_at: db::now(),
        meta: Some(meta),
    };
    db::upsert_published(database, &row)?;
    db::replace_published_files(database, filename, &files)?;
    regenerate_index(database, config)?;
    let recipe = db::package_by_name(database, &row.name)?;
    let state = match recipe {
        Some(recipe) => status::derive_with_epoch(
            recipe.epoch,
            &recipe.version,
            &recipe.release,
            Some((row.epoch, &row.version, &row.release)),
        ),
        None => BuildState::Missing,
    };
    Ok(Receipt {
        filename: row.filename,
        repo_path: row.repo_path,
        name: row.name,
        version: row.version,
        release: row.release,
        size: row.size,
        sha256: row.sha256,
        state,
    })
}

pub fn unpublish(database: &db::Database, config: &Config, filename: &str) -> Result<()> {
    if !valid_filename(filename) {
        bail!("invalid package filename '{filename}'")
    }
    let Some(row) = db::delete_published(database, filename)? else {
        bail!("'{filename}' is not published")
    };
    remove_if_file(&config.repo_dir.join(&row.repo_path))?;
    regenerate_index(database, config)
}

fn remove_if_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

/// Name-compatible alias for callers that only need a rebuild.
pub fn regenerate_indexes(database: &db::Database, config: &Config) -> Result<()> {
    regenerate_index(database, config)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "recipedia-sage-repo-{}-{}",
            std::process::id(),
            db::now()
        ))
    }

    #[test]
    fn ingest_builds_the_sage_network_repo_layout() {
        let root = test_root();
        let _ = std::fs::remove_dir_all(&root);
        let stage = root.join("stage");
        std::fs::create_dir_all(stage.join(".METADATA")).unwrap();
        std::fs::create_dir_all(stage.join("data/usr/bin")).unwrap();
        std::fs::write(stage.join("data/usr/bin/demo"), b"demo").unwrap();
        let records = sage_archive::build_file_index(&stage.join("data")).unwrap();
        std::fs::write(
            stage.join(".METADATA/files.idx"),
            sage_archive::format_file_index(&records),
        )
        .unwrap();
        let manifest = sage_archive::PackageManifest {
            schema_version: 1,
            name: "demo".into(),
            slot: "0".into(),
            version: "1.0".into(),
            release: 1,
            epoch: 0,
            arch: "amd64".into(),
            channel: "system".into(),
            description: "demo".into(),
            license: "MIT".into(),
            dependencies: Vec::new(),
            provides: vec!["cmd:demo".into()],
            conflicts: Vec::new(),
            features: Vec::new(),
            installed_size: 4,
            build_time: 1,
            managed_build_tools: Vec::new(),
        };
        std::fs::write(
            stage.join(".METADATA/manifest.toml"),
            toml::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let upload = root.join("demo-1.0-1-amd64.pkg.tar.zst");
        sage_archive::create_package(&stage, &upload, 1).unwrap();
        let config = Config {
            listen: String::new(),
            db_path: root.join("database"),
            state_dir: root.clone(),
            repo_dir: root.join("repo"),
            repo_channel: "main".into(),
            repo_signing_key: root.join("repo/signing.key"),
            git_url: String::new(),
            webhook_secret: None,
            poll_secs: 600,
            repo_base: String::new(),
            frontend_url: String::new(),
        };
        let database = db::open(&config.db_path).unwrap();
        let sha256 = sha256_file(&upload).unwrap();
        let receipt = ingest(
            &database,
            &config,
            &upload,
            "demo-1.0-1-amd64.pkg.tar.zst",
            &sha256,
            Some(&sha256),
            "test",
        )
        .unwrap();
        assert_eq!(receipt.repo_path, "system/demo-1.0-1-amd64.pkg.tar.zst");
        let index = config.repo_dir.join("system/index.mdb");
        let reader = sage_repo::RepositoryIndex::open(&index).unwrap();
        assert_eq!(reader.releases("demo", "0").unwrap().len(), 1);
        assert_eq!(reader.providers("cmd:demo").unwrap(), vec!["demo:0"]);
        assert!(index.with_extension("mdb.zst").exists());
        assert!(index.with_extension("mdb.sig").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}

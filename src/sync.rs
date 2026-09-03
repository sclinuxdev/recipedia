//! Sage 0.4 recipe synchronization.
//!
//! The source tree is intentionally treated as Sage treats it: every
//! `recipe.toml` is loaded through `sage-build::RecipeSpec`, so validation and
//! package semantics come from one implementation. Recipedia only adds the
//! source path and Git provenance needed by its presentation layer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::db::{self, RecipeRecord};
use crate::model::canonical_arch;

const ARCHES: &[&str] = &[
    "amd64", "x86_64", "aarch64", "arm64", "armv7", "arm", "armhf", "armv7l", "riscv64", "any",
    "noarch",
];
type RecipeCoordinate = (String, String, String, String, u32, String, u32);

/// Clone or fast-forward the read-only mirror and return its current commit.
pub fn update_mirror(git_url: &str, git_dir: &Path) -> Result<String> {
    if git_dir.join(".git").exists() {
        run(git_dir, "git", &["fetch", "origin", "--prune"])?;
        let branch = run(
            git_dir,
            "git",
            &["symbolic-ref", "refs/remotes/origin/HEAD"],
        )
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "origin/main".to_string());
        run(git_dir, "git", &["reset", "--hard", &branch])?;
    } else {
        let name = git_dir
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mirror".into());
        let parent = git_dir.parent().context("git mirror has no parent")?;
        std::fs::create_dir_all(parent)?;
        run(parent, "git", &["clone", "--depth", "1", git_url, &name])?;
    }
    Ok(run(git_dir, "git", &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

/// Cheap remote HEAD probe used by the poll loop.
pub fn remote_head(git_url: &str) -> Result<String> {
    let out = run(
        Path::new("."),
        "git",
        &["ls-remote", git_url, "refs/heads/main"],
    )?;
    out.split_whitespace()
        .next()
        .map(str::to_owned)
        .context("ls-remote produced no SHA")
}

fn changed_arches(git_dir: &Path, previous: Option<&str>, current: &str) -> Result<Vec<String>> {
    let paths = match previous {
        Some(old) if old == current => String::new(),
        Some(old) if commit_exists(git_dir, old)? => {
            run(git_dir, "git", &["diff", "--name-only", old, current])?
        }
        _ => run(git_dir, "git", &["ls-tree", "-r", "--name-only", current])?,
    };
    Ok(arches_from_paths(&paths))
}

fn commit_exists(git_dir: &Path, commit: &str) -> Result<bool> {
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(false);
    }
    let output = std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .current_dir(git_dir)
        .output()
        .context("checking previous Git commit")?;
    Ok(output.status.success())
}

fn arches_from_paths(paths: &str) -> Vec<String> {
    let mut arches = BTreeSet::new();
    for path in paths.lines() {
        for component in Path::new(path).components() {
            let component = component.as_os_str().to_string_lossy();
            if ARCHES.contains(&component.as_ref()) {
                arches.insert(canonical_arch(&component).to_string());
            }
        }
    }
    arches.into_iter().collect()
}

/// Load every Sage recipe below `git_dir`, without imposing an old category
/// or version-directory layout. Duplicate coordinates are rejected because a
/// single Sage repository cannot publish two definitions for one package key.
pub fn collect_recipes(git_dir: &Path, origin_url: &str) -> Result<Vec<RecipeRecord>> {
    let mut records: BTreeMap<RecipeCoordinate, RecipeRecord> = BTreeMap::new();
    let mut stack = vec![git_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| dir.display().to_string())? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == ".git") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.file_name().is_none_or(|name| name != "recipe.toml") {
                continue;
            }
            let spec = sage_build::RecipeSpec::load(&path)
                .with_context(|| format!("loading Sage recipe {}", path.display()))?;
            let package = spec.package.clone();
            let relative = path
                .strip_prefix(git_dir)
                .context("recipe outside mirror")?
                .to_string_lossy()
                .to_string();
            let category = PathBuf::from(&relative)
                .components()
                .next()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "other".into());
            let source = spec.source_inputs().next();
            let key = (
                package.channel.clone(),
                package.name.clone(),
                package.slot.clone(),
                canonical_arch(&package.arch).to_string(),
                package.epoch,
                package.version.clone(),
                package.release,
            );
            if let Some(previous) = records.get(&key) {
                bail!(
                    "duplicate Sage package {}:{}:{} at {} and {}",
                    package.channel,
                    package.name,
                    package.slot,
                    previous.recipe_path,
                    relative
                );
            }
            records.insert(
                key,
                RecipeRecord {
                    package,
                    origin: origin_url.to_string(),
                    category,
                    recipe_path: relative,
                    git_commit: String::new(),
                    source_url: source.map(|source| source.url.clone()).unwrap_or_default(),
                    source_sha256: source
                        .map(|source| source.sha256.clone())
                        .unwrap_or_default(),
                },
            );
        }
    }
    Ok(records.into_values().collect())
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub count: usize,
    pub changed_arches: Vec<String>,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let changed = if self.changed_arches.is_empty() {
            "none".to_string()
        } else {
            self.changed_arches.join(", ")
        };
        format!(
            "{} Sage recipes; changed architectures: {changed}",
            self.count
        )
    }
}

pub fn run_sync(database: &db::Database, config: &Config, trigger: &str) -> Result<SyncReport> {
    let started = db::now();
    let result = sync_inner(database, config);
    match &result {
        Ok((commit, report)) => {
            db::log_sync(database, trigger, commit, started, true, &report.summary())?;
        }
        Err(error) => {
            db::log_sync(database, trigger, "", started, false, &format!("{error:#}"))?;
        }
    }
    result.map(|(_, report)| report)
}

fn sync_inner(database: &db::Database, config: &Config) -> Result<(String, SyncReport)> {
    let git_dir = config.git_dir();
    let previous = db::meta_get(database, "last_commit")?;
    let commit = update_mirror(&config.git_url, &git_dir)?;
    let changed_arches = changed_arches(&git_dir, previous.as_deref(), &commit)
        .context("determining changed architectures")?;
    let mut records = collect_recipes(&git_dir, &config.git_url)?;
    for record in &mut records {
        record.git_commit = commit.clone();
    }
    db::rebuild_packages(database, &records)?;
    db::meta_set(database, "last_commit", &commit)?;
    db::meta_set(database, "last_changed_arches", &changed_arches.join(","))?;
    Ok((
        commit,
        SyncReport {
            count: records.len(),
            changed_arches,
        },
    ))
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("spawning {program} {args:?}"))?;
    if !output.status.success() {
        bail!(
            "{program} {args:?} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_architectures_are_found_in_sage_tree_paths() {
        assert_eq!(
            arches_from_paths(
                "recipes/gcc/amd64/recipe.toml\nrecipes/base/any/recipe.toml\n\
                 recipes/zlib/aarch64/service.toml\nREADME.md\n"
            ),
            ["aarch64", "amd64", "any"]
        );
    }

    #[test]
    fn collector_uses_sage_recipe_spec_without_a_legacy_layout() {
        let root = std::env::temp_dir().join(format!(
            "recipedia-sage-recipes-{}-{}",
            std::process::id(),
            db::now()
        ));
        let recipe = root.join("packages/demo/recipe.toml");
        std::fs::create_dir_all(recipe.parent().unwrap()).unwrap();
        std::fs::write(
            &recipe,
            r#"schema_version = 1
[package]
name = "demo"
slot = "0"
version = "1.0"
release = 1
arch = "amd64"
channel = "system"
description = "demo"
license = "MIT"
"#,
        )
        .unwrap();
        let records = collect_recipes(&root, "https://example.invalid/sage").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].package.name, "demo");
        assert_eq!(records[0].recipe_path, "packages/demo/recipe.toml");
        let _ = std::fs::remove_dir_all(root);
    }
}

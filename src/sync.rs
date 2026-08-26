use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::db::{self, RecipeRecord};
use crate::model::{canonical_arch, Recipe};

/// Categories of the nine-way tree. When the recipes repo is restructured
/// (P0), the first path component becomes one of these; until then the flat
/// tree reports every package under `misc`.
pub const CATEGORIES: [&str; 9] = [
    "system", "devel", "lib", "net", "security", "media", "text", "utils", "other",
];

/// Clone or fast-forward the read-only mirror; returns the commit SHA after
/// the update. Public repo -- plain https, no credentials involved.
pub fn update_mirror(git_url: &str, git_dir: &Path) -> Result<String> {
    if git_dir.join(".git").exists() {
        run(git_dir, "git", &["fetch", "origin", "--prune"])?;
        run(git_dir, "git", &["reset", "--hard", "origin/main"])?;
    } else {
        // One mirror directory per tree, named by its arch
        // (<state>/git/amd64, <state>/git/aarch64).
        let name = git_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "mirror".to_string());
        if let Some(parent) = git_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        run(
            git_dir.parent().unwrap(),
            "git",
            &["clone", "--depth", "1", git_url, &name],
        )?;
    }
    let head = run(git_dir, "git", &["rev-parse", "HEAD"])?;
    Ok(head.trim().to_string())
}

/// Remote HEAD SHA without touching the mirror -- the poll loop's cheap
/// "did anything change" probe.
pub fn remote_head(git_url: &str) -> Result<String> {
    let out = run(
        Path::new("."),
        "git",
        &["ls-remote", git_url, "refs/heads/main"],
    )?;
    let sha = out
        .split_whitespace()
        .next()
        .context("ls-remote produced no SHA")?;
    Ok(sha.to_string())
}

/// Architectures touched between two canonical-tree commits. The architecture
/// is structural (`<category>/<name>/<arch>/...`), so this remains accurate
/// even when a recipe was deleted and is no longer available to parse.
fn changed_arches(git_dir: &Path, previous: Option<&str>, current: &str) -> Result<Vec<String>> {
    let paths = match previous {
        Some(old) if old == current => String::new(),
        Some(old) if commit_exists(git_dir, old)? => {
            run(git_dir, "git", &["diff", "--name-only", old, current])?
        }
        // The SQLite cache can outlive a replaced or re-cloned shallow Git
        // mirror. Without the old object an incremental diff is impossible;
        // treat the current tree as a full change and let the atomic recipe
        // rebuild restore a valid baseline instead of failing forever.
        Some(_) => run(git_dir, "git", &["ls-tree", "-r", "--name-only", current])?,
        None => run(git_dir, "git", &["ls-tree", "-r", "--name-only", current])?,
    };
    Ok(arches_from_paths(&paths))
}

fn commit_exists(git_dir: &Path, commit: &str) -> Result<bool> {
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(false);
    }
    let object = format!("{commit}^{{commit}}");
    let output = std::process::Command::new("git")
        .args(["cat-file", "-e", &object])
        .current_dir(git_dir)
        .output()
        .context("checking previous Git commit")?;
    Ok(output.status.success())
}

fn arches_from_paths(paths: &str) -> Vec<String> {
    let mut arches = BTreeSet::new();
    for path in paths.lines() {
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() >= 3 && CATEGORIES.contains(&parts[0]) {
            arches.insert(canonical_arch(parts[2]).to_string());
        }
    }
    arches.into_iter().collect()
}

/// Walk every recipe.toml under the canonical mirror. Its single-source layout
/// is `<category>/<name>/<arch>/<name>-<version>-<release>/recipe.toml`.
/// A package may keep several version directories side by side; only a true
/// collision (same name+version+release in two places) is rejected.
pub fn collect_recipes(git_dir: &Path, origin_url: &str) -> Result<Vec<RecipeRecord>> {
    let mut out: BTreeMap<(String, String, String, String), RecipeRecord> = BTreeMap::new();
    let mut stack = vec![git_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).with_context(|| dir.display().to_string())? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == "recipe.toml") {
                let mut record = parse_recipe_at(git_dir, &path)?;
                record.origin = origin_url.to_string();
                let key = (
                    record.recipe.name.clone(),
                    record.arch.clone(),
                    record.recipe.version.clone(),
                    record.recipe.release.clone(),
                );
                if let Some(dup) = out.get(&key) {
                    bail!(
                        "duplicate package '{}-{}-{}-{}' at {}/{} and {}/{}",
                        record.recipe.name,
                        record.arch,
                        record.recipe.version,
                        record.recipe.release,
                        dup.origin,
                        dup.recipe_path,
                        origin_url,
                        record.recipe_path
                    );
                }
                out.insert(key, record);
            }
        }
    }
    Ok(out.into_values().collect())
}

fn parse_recipe_at(git_dir: &Path, path: &Path) -> Result<RecipeRecord> {
    let rel = path
        .strip_prefix(git_dir)
        .context("recipe outside mirror")?
        .to_string_lossy()
        .to_string();
    let components: Vec<_> = Path::new(&rel)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let (category, path_name, path_arch, identity, recipe_path) = match components.as_slice() {
        [cat, name, arch, identity, file] if CATEGORIES.contains(&cat.as_str()) => (
            cat.clone(),
            name.clone(),
            canonical_arch(arch).to_string(),
            identity.clone(),
            format!("{cat}/{name}/{arch}/{identity}/{file}"),
        ),
        other => bail!("unexpected recipe layout: {}", other.join("/")),
    };
    let text = std::fs::read_to_string(path)?;
    let recipe = Recipe::from_toml(&text).with_context(|| format!("parsing {rel}"))?;
    if recipe.name != path_name {
        bail!(
            "recipe name '{}' does not match path package '{}' at {rel}",
            recipe.name,
            path_name
        );
    }
    if recipe.arch.is_empty() {
        bail!("recipe must declare arch matching its single-tree path at {rel}");
    }
    if canonical_arch(&recipe.arch) != path_arch {
        bail!(
            "recipe arch '{}' does not match path arch '{}' at {rel}",
            recipe.arch,
            path_arch
        );
    }
    let expected_identity = format!("{}-{}-{}", recipe.name, recipe.version, recipe.release);
    if identity != expected_identity {
        bail!(
            "recipe identity '{expected_identity}' does not match directory '{identity}' at {rel}"
        );
    }
    Ok(RecipeRecord {
        recipe,
        arch: path_arch,
        origin: String::new(),
        category,
        recipe_path,
        git_commit: String::new(),
    })
}

fn run(dir: &Path, prog: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("spawning {prog} {args:?}"))?;
    if !out.status.success() {
        bail!(
            "{prog} {args:?} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Full sync cycle from the one canonical tree, then atomically swap the cache.
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
        format!("{} recipes; changed architectures: {changed}", self.count)
    }
}

pub fn run_sync(conn: &rusqlite::Connection, config: &Config, trigger: &str) -> Result<SyncReport> {
    let started = db::now();
    let result = sync_inner(conn, config);
    match &result {
        Ok((commit, report)) => {
            db::log_sync(conn, trigger, commit, started, true, &report.summary())?;
        }
        Err(err) => {
            db::log_sync(conn, trigger, "", started, false, &format!("{err:#}"))?;
        }
    }
    result.map(|(_, report)| report)
}

fn sync_inner(conn: &rusqlite::Connection, config: &Config) -> Result<(String, SyncReport)> {
    let git_dir = config.git_dir();
    let previous = db::meta_get(conn, "last_commit")?;
    let commit = update_mirror(&config.git_url, &git_dir)?;
    let changed_arches = changed_arches(&git_dir, previous.as_deref(), &commit)
        .context("determining changed architectures")?;
    let mut records = collect_recipes(&git_dir, &config.git_url)?;
    for record in &mut records {
        record.git_commit = commit.clone();
    }
    db::meta_set(conn, "last_commit", &commit)?;
    db::meta_set(conn, "last_changed_arches", &changed_arches.join(","))?;
    db::rebuild_packages(conn, &records)?;
    let count = records.len();
    Ok((
        commit,
        SyncReport {
            count,
            changed_arches,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_architectures_come_from_canonical_paths() {
        assert_eq!(
            arches_from_paths(
                "devel/gcc/amd64/gcc-16.2.0-1/recipe.toml\n\
                 system/base/any/base-1.0.0-1/recipe.toml\n\
                 lib/zlib/aarch64/zlib-1.3.2-2/service.toml\n\
                 README.md\n"
            ),
            ["aarch64", "amd64", "any"]
        );
    }

    #[test]
    fn missing_previous_commit_falls_back_to_the_full_tree() {
        let root = std::env::temp_dir().join(format!(
            "recipedia-missing-sync-baseline-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let recipe = root.join("devel/demo/amd64/demo-1.0.0-1/recipe.toml");
        std::fs::create_dir_all(recipe.parent().unwrap()).unwrap();
        std::fs::write(&recipe, "name = \"demo\"\n").unwrap();
        run(&root, "git", &["init"]).unwrap();
        run(&root, "git", &["add", "."]).unwrap();
        run(
            &root,
            "git",
            &[
                "-c",
                "user.name=Recipedia Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        let current = run(&root, "git", &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        assert_eq!(
            changed_arches(
                &root,
                Some("5e0cee6a8836299985d3c8022701a6c7f9d7a921"),
                &current,
            )
            .unwrap(),
            ["amd64"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn collector_requires_the_single_tree_identity() {
        let root =
            std::env::temp_dir().join(format!("recipedia-single-tree-{}", std::process::id()));
        let recipe_dir = root.join("devel/demo/amd64/demo-1.2.3-4");
        std::fs::create_dir_all(&recipe_dir).unwrap();
        std::fs::write(
            recipe_dir.join("recipe.toml"),
            r#"schema_version = 1
[package]
name = "demo"
version = "1.2.3"
release = "4"
description = "demo"
license = "MIT"
channel = "system"
arch = "amd64"
"#,
        )
        .unwrap();
        let records = collect_recipes(&root, "https://example.invalid/recipes").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].arch, "amd64");
        assert_eq!(
            records[0].recipe_path,
            "devel/demo/amd64/demo-1.2.3-4/recipe.toml"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

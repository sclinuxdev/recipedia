use std::collections::BTreeMap;
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
    let out = run(Path::new("."), "git", &["ls-remote", git_url, "refs/heads/main"])?;
    let sha = out
        .split_whitespace()
        .next()
        .context("ls-remote produced no SHA")?;
    Ok(sha.to_string())
}

/// Walk every recipe.toml under the mirror and parse it. Both tree shapes are
/// accepted: the current flat `<name>/<ver>-<rel>/recipe.toml` (category
/// `misc`) and the nine-category `<cat>/<name>/<ver>-<rel>/recipe.toml`.
/// A package may keep several version directories side by side; only a true
/// collision (same name+version+release in two places) is rejected.
/// Walk one tree. The status key gains an arch dimension: the effective
/// architecture is the declared `arch` (canonicalized) when present, else the
/// tree's own -- so an undeclared recipe in recipes.aarch64 is aarch64.
pub fn collect_recipes(
    git_dir: &Path,
    source_arch: &str,
    origin_url: &str,
) -> Result<Vec<RecipeRecord>> {
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
                let eff = if record.recipe.arch.is_empty() {
                    source_arch.to_string()
                } else {
                    record.recipe.arch.clone()
                };
                record.arch = canonical_arch(&eff).to_string();
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
    // <cat>/<name>/<ver>/recipe.toml vs <name>/<ver>/recipe.toml
    let (category, recipe_path) = match components.as_slice() {
        [cat, name, ver, file] if CATEGORIES.contains(&cat.as_str()) => {
            (cat.clone(), format!("{cat}/{name}/{ver}/{file}"))
        }
        [name, ver, file] => ("misc".to_string(), format!("{name}/{ver}/{file}")),
        other => bail!("unexpected recipe layout: {}", other.join("/")),
    };
    let text = std::fs::read_to_string(path)?;
    let recipe = Recipe::from_toml(&text)
        .with_context(|| format!("parsing {rel}"))?;
    Ok(RecipeRecord {
        recipe,
        // Filled in by the caller once the source tree's arch/commit bind.
        arch: String::new(),
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

/// Full sync cycle across every configured tree: update each mirror, parse,
/// then atomically swap the cache table once for the combined world.
pub fn run_sync(conn: &rusqlite::Connection, config: &Config, trigger: &str) -> Result<usize> {
    let started = db::now();
    let result = sync_inner(conn, config);
    match &result {
        Ok((commits, count)) => {
            let summary = format!("{count} recipes from {} trees", commits.len());
            db::log_sync(conn, trigger, &commits.join(","), started, true, &summary)?;
        }
        Err(err) => {
            db::log_sync(conn, trigger, "", started, false, &format!("{err:#}"))?;
        }
    }
    result.map(|(_, count)| count)
}

fn sync_inner(conn: &rusqlite::Connection, config: &Config) -> Result<(Vec<String>, usize)> {
    let mut records: Vec<RecipeRecord> = Vec::new();
    let mut commits = Vec::new();
    for source in &config.git_sources {
        let git_dir = config.git_dir(&source.arch);
        let commit = update_mirror(&source.url, &git_dir)?;
        let mut tree = collect_recipes(&git_dir, &source.arch, &source.url)?;
        for r in &mut tree {
            r.git_commit = commit.clone();
        }
        commits.push(format!("{}@{}", source.arch, &commit[..12.min(commit.len())]));
        records.extend(tree);
        db::meta_set(conn, &format!("last_commit:{}", source.arch), &commit)?;
    }
    db::rebuild_packages(conn, &records)?;
    Ok((commits, records.len()))
}

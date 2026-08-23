use std::path::PathBuf;

/// One recipes git tree. Each tree carries one architecture family; the site
/// aggregates all of them so the status board shows every arch in one place.
#[derive(Debug, Clone)]
pub struct GitSource {
    /// Architecture this tree targets (`amd64`, `aarch64`, ...). Recipes that
    /// do not declare `arch` inherit it.
    pub arch: String,
    pub url: String,
}

/// Runtime configuration. Everything is environment-driven with working
/// defaults so a dev checkout runs with zero setup and the systemd unit stays
/// one EnvironmentFile away.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub db_path: PathBuf,
    /// Root holding the read-only git mirrors and the published packages.
    pub state_dir: PathBuf,
    /// Published binary packages: `*.pkg.tar.zst` + regenerated index.toml.
    pub repo_dir: PathBuf,
    /// Every recipes tree being aggregated (amd64 + aarch64 by default).
    pub git_sources: Vec<GitSource>,
    pub webhook_secret: Option<String>,
    /// Fallback poll interval in seconds (webhook is the fast path).
    pub poll_secs: u64,
    /// Public base URL the frontend links repo files at (e.g.
    /// `https://repo.example.com`); empty keeps same-origin `/repo/...`.
    pub repo_base: String,
    /// Public origin of the main site's pages (e.g. `https://rp.example.com`)
    /// so the shared nav points there even when served from the repo domain;
    /// empty keeps same-origin root-relative links.
    pub frontend_url: String,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Parse `RECIPEEDIA_GIT_URLS`: comma-separated `arch=url` pairs; a bare URL
/// derives its arch from a `.amd64`/`.aarch64` repo-name suffix, defaulting
/// to `amd64`.
pub fn parse_git_urls(spec: &str) -> Vec<GitSource> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (arch, url) = match part.split_once('=') {
            Some((a, u)) => (a.trim().to_string(), u.trim().to_string()),
            None => {
                let url = part.to_string();
                // `.aarch64` names its family; every other spelling
                // (recipes.amd64, bare `recipes`, ...) defaults to amd64.
                let arch = if url.trim_end_matches('/').ends_with(".aarch64") {
                    "aarch64"
                } else {
                    "amd64"
                };
                (arch.to_string(), url)
            }
        };
        if !url.is_empty() && !out.iter().any(|s: &GitSource| s.arch == arch) {
            out.push(GitSource { arch, url });
        }
    }
    out
}

impl Config {
    pub fn from_env() -> Self {
        let state_dir = PathBuf::from(env_or("RECIPEEDIA_STATE_DIR", "/srv/recipedia"));
        let git_urls = env_or(
            "RECIPEEDIA_GIT_URLS",
            // Legacy singular variable still works for a single-tree deploy.
            &env_or(
                "RECIPEEDIA_GIT_URL",
                "amd64=https://github.com/sclinuxdev/recipes.amd64,aarch64=https://github.com/sclinuxdev/recipes.aarch64",
            ),
        );
        Self {
            listen: env_or("RECIPEEDIA_LISTEN", "127.0.0.1:8300"),
            db_path: PathBuf::from(env_or(
                "RECIPEEDIA_DB",
                &state_dir.join("recipedia.sqlite").display().to_string(),
            )),
            repo_dir: state_dir.join("repo"),
            state_dir,
            git_sources: parse_git_urls(&git_urls),
            webhook_secret: std::env::var("RECIPEEDIA_WEBHOOK_SECRET").ok(),
            poll_secs: env_or("RECIPEEDIA_POLL_SECS", "600").parse().unwrap_or(600),
            repo_base: env_or("RECIPEEDIA_REPO_URL", "").trim_end_matches('/').to_string(),
            frontend_url: env_or("RECIPEEDIA_FRONTEND_URL", "").trim_end_matches('/').to_string(),
        }
    }

    /// Read-only mirror directory of one recipes tree, keyed by arch.
    pub fn git_dir(&self, arch: &str) -> PathBuf {
        self.state_dir.join("git").join(arch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_covers_both_trees() {
        let sources = parse_git_urls(
            "amd64=https://github.com/sclinuxdev/recipes.amd64,aarch64=https://github.com/sclinuxdev/recipes.aarch64",
        );
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].arch, "amd64");
        assert_eq!(sources[1].arch, "aarch64");
    }

    #[test]
    fn bare_url_infers_arch_from_suffix() {
        let sources = parse_git_urls("https://github.com/sclinuxdev/recipes.aarch64");
        assert_eq!(sources[0].arch, "aarch64");
        let sources = parse_git_urls("https://github.com/sclinuxdev/recipes");
        assert_eq!(sources[0].arch, "amd64");
    }
}

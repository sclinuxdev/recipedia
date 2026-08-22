use std::path::PathBuf;

/// Runtime configuration. Everything is environment-driven with working
/// defaults so a dev checkout runs with zero setup and the systemd unit stays
/// one EnvironmentFile away.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub db_path: PathBuf,
    /// Root holding the read-only git mirror and the published packages.
    pub state_dir: PathBuf,
    /// Published binary packages: `*.pkg.tar.zst` + regenerated index.toml.
    pub repo_dir: PathBuf,
    pub git_url: String,
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

impl Config {
    pub fn from_env() -> Self {
        let state_dir = PathBuf::from(env_or("RECIPEEDIA_STATE_DIR", "/srv/recipedia"));
        Self {
            listen: env_or("RECIPEEDIA_LISTEN", "127.0.0.1:8300"),
            db_path: PathBuf::from(env_or(
                "RECIPEEDIA_DB",
                &state_dir.join("recipedia.sqlite").display().to_string(),
            )),
            repo_dir: state_dir.join("repo"),
            state_dir,
            git_url: env_or("RECIPEEDIA_GIT_URL", "https://github.com/sclinuxdev/recipes.amd64"),
            webhook_secret: std::env::var("RECIPEEDIA_WEBHOOK_SECRET").ok(),
            poll_secs: env_or("RECIPEEDIA_POLL_SECS", "600").parse().unwrap_or(600),
            repo_base: env_or("RECIPEEDIA_REPO_URL", "").trim_end_matches('/').to_string(),
            frontend_url: env_or("RECIPEEDIA_FRONTEND_URL", "").trim_end_matches('/').to_string(),
        }
    }

    /// Read-only mirror of the recipes repository.
    pub fn git_dir(&self) -> PathBuf {
        self.state_dir.join("git").join("sclinux")
    }
}

use std::path::PathBuf;

/// Runtime configuration. Everything is environment-driven with working
/// defaults so a dev checkout runs with zero setup and the systemd unit stays
/// one EnvironmentFile away.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub db_path: PathBuf,
    /// Root holding the read-only git mirrors and the published packages.
    pub state_dir: PathBuf,
    /// Published binary packages and Sage network-repository indexes.
    pub repo_dir: PathBuf,
    /// Logical root channel for unqualified package channels (`system` becomes
    /// `main/system`; the root itself is represented by ChannelsConfig.url).
    pub repo_channel: String,
    /// Raw 32-byte Ed25519 private key used to sign Sage index.mdb files.
    pub repo_signing_key: PathBuf,
    /// The one canonical recipes tree. Architecture is part of each recipe's
    /// path and declaration, never inferred from a repository name.
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

const DEFAULT_GIT_URL: &str = "https://github.com/sclinuxdev/recipes";

impl Config {
    pub fn from_env() -> Self {
        let state_dir = PathBuf::from(env_or("RECIPEEDIA_STATE_DIR", "/srv/recipedia"));
        Self {
            listen: env_or("RECIPEEDIA_LISTEN", "127.0.0.1:8300"),
            db_path: PathBuf::from(env_or(
                "RECIPEEDIA_DB",
                &state_dir.join("database").display().to_string(),
            )),
            repo_dir: state_dir.join("repo"),
            repo_channel: env_or("RECIPEEDIA_REPO_CHANNEL", "main"),
            repo_signing_key: std::env::var("RECIPEEDIA_REPO_SIGNING_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| state_dir.join("signing.key")),
            state_dir,
            git_url: env_or("RECIPEEDIA_GIT_URL", DEFAULT_GIT_URL),
            webhook_secret: std::env::var("RECIPEEDIA_WEBHOOK_SECRET").ok(),
            poll_secs: env_or("RECIPEEDIA_POLL_SECS", "600").parse().unwrap_or(600),
            repo_base: env_or("RECIPEEDIA_REPO_URL", "")
                .trim_end_matches('/')
                .to_string(),
            frontend_url: env_or("RECIPEEDIA_FRONTEND_URL", "")
                .trim_end_matches('/')
                .to_string(),
        }
    }

    /// Read-only mirror of the canonical recipes tree.
    pub fn git_dir(&self) -> PathBuf {
        self.state_dir.join("git").join("recipes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_canonical_single_tree() {
        assert_eq!(DEFAULT_GIT_URL, "https://github.com/sclinuxdev/recipes");
        let config = Config {
            listen: String::new(),
            db_path: PathBuf::new(),
            state_dir: PathBuf::from("/state"),
            repo_dir: PathBuf::new(),
            repo_channel: "main".into(),
            repo_signing_key: PathBuf::new(),
            git_url: DEFAULT_GIT_URL.to_string(),
            webhook_secret: None,
            poll_secs: 600,
            repo_base: String::new(),
            frontend_url: String::new(),
        };
        assert!(config.git_dir().ends_with("git/recipes"));
    }
}

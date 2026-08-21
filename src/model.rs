use anyhow::{Context, Result};
use serde::Serialize;

/// A parsed recipe. Mirrors sage's extraction semantics: every field may live
/// at the document root, under `[package]`, or under `[source]` -- the TOML
/// section-attribution rule means later bare keys land in whichever table was
/// opened last, so the parser merges all three scopes instead of trusting one.
#[derive(Debug, Clone, Serialize)]
pub struct Recipe {
    pub name: String,
    pub version: String,
    pub release: String,
    pub description: String,
    pub license: String,
    pub channel: String,
    pub source_url: String,
    pub source_sha256: String,
    /// `[["name", "req"]]`-style pairs: request name split from constraint so
    /// reverse-dependency lookups stay exact.
    pub dependencies: Vec<Dep>,
    pub build_dependencies: Vec<Dep>,
    pub provides: Vec<String>,
    pub conffiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Dep {
    pub name: String,
    /// Raw constraint tail (`">= 15.3.0"`, empty when unconstrained).
    pub req: String,
}

/// `"gcc >= 15.3.0"` -> name `gcc`, req `>= 15.3.0`.
pub fn parse_dep(raw: &str) -> Dep {
    let raw = raw.trim();
    match raw.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '=' | '!')) {
        Some(idx) => {
            let (name, rest) = raw.split_at(idx);
            Dep {
                name: name.trim().to_string(),
                req: rest.trim().to_string(),
            }
        }
        None => Dep {
            name: raw.to_string(),
            req: String::new(),
        },
    }
}

fn table_strings(scope: &toml::Table, key: &str) -> Vec<String> {
    scope
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn table_str(scope: &toml::Table, key: &str) -> Option<String> {
    scope.get(key).and_then(|v| v.as_str().map(str::to_string))
}

impl Recipe {
    pub fn from_toml(text: &str) -> Result<Self> {
        let doc: toml::Table = toml::from_str(text).context("recipe.toml is not valid TOML")?;
        let pkg = doc.get("package").and_then(|v| v.as_table());
        let src = doc.get("source").and_then(|v| v.as_table());

        // Three-scope merge: root first, then [package], then [source] --
        // later scopes win, matching sage's parser.
        let mut name = table_str(&doc, "name");
        let mut version = table_str(&doc, "version");
        let mut release = table_str(&doc, "release");
        let mut description = table_str(&doc, "description");
        let mut license = table_str(&doc, "license");
        let mut channel = table_str(&doc, "channel");
        let mut source_url = table_str(&doc, "url");
        let mut source_sha256 = table_str(&doc, "sha256");
        let mut dependencies = table_strings(&doc, "dependencies");
        let mut build_dependencies = table_strings(&doc, "build_dependencies");
        let mut provides = table_strings(&doc, "provides");
        let mut conffiles = table_strings(&doc, "conffiles");
        for scope in [pkg, src].into_iter().flatten() {
            name = table_str(scope, "name").or(name);
            version = table_str(scope, "version").or(version);
            release = table_str(scope, "release").or(release);
            description = table_str(scope, "description").or(description);
            license = table_str(scope, "license").or(license);
            channel = table_str(scope, "channel").or(channel);
            source_url = table_str(scope, "url").or(source_url);
            source_sha256 = table_str(scope, "sha256").or(source_sha256);
            // Arrays append across scopes, matching sage's parser.
            dependencies.extend(table_strings(scope, "dependencies"));
            build_dependencies.extend(table_strings(scope, "build_dependencies"));
            provides.extend(table_strings(scope, "provides"));
            conffiles.extend(table_strings(scope, "conffiles"));
        }

        let name = name.context("recipe has no name")?;
        let version = version.context("recipe has no version")?;
        Ok(Self {
            name,
            version,
            release: release.unwrap_or_else(|| "1".into()),
            description: description.unwrap_or_default(),
            license: license.unwrap_or_default(),
            channel: channel.unwrap_or_else(|| "system".into()),
            source_url: source_url.unwrap_or_default(),
            source_sha256: source_sha256.unwrap_or_default(),
            dependencies: dependencies.iter().map(|s| parse_dep(s)).collect(),
            build_dependencies: build_dependencies.iter().map(|s| parse_dep(s)).collect(),
            provides,
            conffiles,
        })
    }
}

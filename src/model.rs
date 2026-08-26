use anyhow::{Context, Result};
use serde::Serialize;

/// A parsed recipe. Mirrors sage's extraction semantics: every field may live
/// at the document root, under `[package]`, or under `[source]` -- the TOML
/// section-attribution rule means later bare keys land in whichever table was
/// opened last, so the parser merges all three scopes instead of trusting one.
#[derive(Debug, Clone, Serialize)]
pub struct Recipe {
    pub name: String,
    /// Declared architecture, canonicalized (`x86_64` -> `amd64`). Empty when
    /// the recipe does not declare one -- callers fall back to the tree's own
    /// architecture.
    pub arch: String,
    pub version: String,
    pub release: String,
    pub description: String,
    pub license: String,
    pub channel: String,
    pub source_url: String,
    pub source_sha256: String,
    /// Optional upstream release feed/page and its capture regex. Both are
    /// present or both are empty.
    pub upstream_url: String,
    pub upstream_version_regex: String,
    /// `[["name", "req"]]`-style pairs: request name split from constraint so
    /// reverse-dependency lookups stay exact.
    pub dependencies: Vec<Dep>,
    pub build_dependencies: Vec<Dep>,
    pub provides: Vec<String>,
    pub conffiles: Vec<String>,
}

/// Canonical architecture spelling: the legacy `x86_64` alias folds into
/// `amd64` so recipe declarations and published artifact names always meet.
pub fn canonical_arch(arch: &str) -> &str {
    match arch.trim() {
        "x86_64" => "amd64",
        other => other.trim(),
    }
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
        // `[source]` may be a single table or a `[[source]]` array of tables;
        // for multi-source recipes element zero is the primary archive -- the
        // one sage unpacks into src/.
        let mut source_scopes: Vec<&toml::Table> = Vec::new();
        match doc.get("source") {
            Some(toml::Value::Table(t)) => source_scopes.push(t),
            Some(toml::Value::Array(a)) => {
                source_scopes.extend(a.iter().filter_map(|v| v.as_table()))
            }
            _ => {}
        }

        // Three-scope merge: root first, then [package], then [source] --
        // later scopes win, matching sage's parser.
        let mut name = table_str(&doc, "name");
        let mut version = table_str(&doc, "version");
        let mut release = table_str(&doc, "release");
        let mut description = table_str(&doc, "description");
        let mut license = table_str(&doc, "license");
        let mut channel = table_str(&doc, "channel");
        let mut arch = table_str(&doc, "arch").map(|a| canonical_arch(&a).to_string());
        let mut source_url = table_str(&doc, "url");
        let mut source_sha256 = table_str(&doc, "sha256");
        let mut dependencies = table_strings(&doc, "dependencies");
        let mut build_dependencies = table_strings(&doc, "build_dependencies");
        let mut provides = table_strings(&doc, "provides");
        let mut conffiles = table_strings(&doc, "conffiles");
        let mut upstream_url = pkg.and_then(|p| table_str(p, "upstream"));
        let mut upstream_version_regex = pkg.and_then(|p| table_str(p, "upstream_regex"));
        for scope in [pkg]
            .into_iter()
            .flatten()
            .chain(source_scopes.iter().copied())
        {
            name = table_str(scope, "name").or(name);
            version = table_str(scope, "version").or(version);
            release = table_str(scope, "release").or(release);
            description = table_str(scope, "description").or(description);
            license = table_str(scope, "license").or(license);
            channel = table_str(scope, "channel").or(channel);
            arch = table_str(scope, "arch")
                .map(|a| canonical_arch(&a).to_string())
                .or(arch);
            source_url = table_str(scope, "url").or(source_url);
            source_sha256 = table_str(scope, "sha256").or(source_sha256);
            // Arrays append across scopes, matching sage's parser.
            dependencies.extend(table_strings(scope, "dependencies"));
            build_dependencies.extend(table_strings(scope, "build_dependencies"));
            provides.extend(table_strings(scope, "provides"));
            conffiles.extend(table_strings(scope, "conffiles"));
        }

        // Recipe v2 may independently constrain any tool package. Expose each
        // declared requirement as a build dependency just as Sage's parser
        // does; executable selection remains Sage policy, not metadata.
        if let Some(toolchain) = doc
            .get("build")
            .and_then(|v| v.as_table())
            .and_then(|b| b.get("toolchain"))
            .and_then(|v| v.as_table())
        {
            for kind in ["compiler", "linker", "rust"] {
                let Some(tool) = toolchain.get(kind).and_then(|v| v.as_table()) else {
                    continue;
                };
                let family = table_str(tool, "family")
                    .with_context(|| format!("build.toolchain.{kind} has no family"))?;
                let package = table_str(tool, "package")
                    .with_context(|| format!("build.toolchain.{kind} has no package"))?;
                let minimum = table_str(tool, "minimum_version")
                    .with_context(|| format!("build.toolchain.{kind} has no minimum_version"))?;
                let supported = match kind {
                    "compiler" => matches!(family.as_str(), "clang" | "gcc"),
                    "linker" => matches!(family.as_str(), "lld" | "mold" | "ld"),
                    _ => family == "rustc",
                };
                anyhow::ensure!(
                    supported,
                    "unsupported build.toolchain.{kind} family '{family}'"
                );
                if kind == "rust" {
                    let system = doc
                        .get("build")
                        .and_then(|v| v.as_table())
                        .and_then(|b| table_str(b, "system"));
                    anyhow::ensure!(
                        system.as_deref() == Some("cargo"),
                        "build.toolchain.rust is valid only for Cargo recipes"
                    );
                }
                if !build_dependencies
                    .iter()
                    .any(|raw| parse_dep(raw).name == package)
                {
                    build_dependencies.push(format!("{package} >= {minimum}"));
                }
            }
        }

        // The loop's last-writer-wins chain would let the final [[source]]
        // element override earlier ones; re-apply element zero so the primary
        // archive's url/sha256 always win.
        if let Some(primary) = source_scopes.first() {
            source_url = table_str(primary, "url").or(source_url);
            source_sha256 = table_str(primary, "sha256").or(source_sha256);
        }

        if let Some(upstream) = doc.get("upstream").and_then(|v| v.as_table()) {
            upstream_url = table_str(upstream, "url").or(upstream_url);
            upstream_version_regex =
                table_str(upstream, "version_regex").or(upstream_version_regex);
        }
        if upstream_url.is_some() != upstream_version_regex.is_some() {
            anyhow::bail!("upstream tracking requires both url and version_regex");
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
            arch: arch.unwrap_or_default(),
            source_url: source_url.unwrap_or_default(),
            source_sha256: source_sha256.unwrap_or_default(),
            upstream_url: upstream_url.unwrap_or_default(),
            upstream_version_regex: upstream_version_regex.unwrap_or_default(),
            dependencies: dependencies.iter().map(|s| parse_dep(s)).collect(),
            build_dependencies: build_dependencies.iter().map(|s| parse_dep(s)).collect(),
            provides,
            conffiles,
        })
    }
    pub fn from_toml_all(text: &str) -> Result<Vec<Self>> {
        let primary = Self::from_toml(text)?;
        let doc: toml::Table = toml::from_str(text).context("recipe.toml is not valid TOML")?;
        let outputs = doc
            .get("build")
            .and_then(|v| v.as_table())
            .and_then(|b| b.get("outputs"))
            .and_then(|v| v.as_array());

        let Some(outputs) = outputs else {
            return Ok(vec![primary]);
        };

        if outputs.is_empty() {
            return Ok(vec![primary]);
        }

        let mut result = Vec::new();
        let mut primary_updated = false;

        for item in outputs {
            let Some(out_table) = item.as_table() else { continue; };
            let out_name = table_str(out_table, "name");
            let Some(out_name) = out_name else { continue; };

            let out_ver = table_str(out_table, "version").unwrap_or_else(|| primary.version.clone());
            let out_rel = table_str(out_table, "release").unwrap_or_else(|| primary.release.clone());
            let out_desc = table_str(out_table, "description").unwrap_or_else(|| primary.description.clone());
            let out_lic = table_str(out_table, "license").unwrap_or_else(|| primary.license.clone());
            let out_channel = table_str(out_table, "channel").unwrap_or_else(|| primary.channel.clone());
            let out_arch = table_str(out_table, "arch")
                .map(|a| canonical_arch(&a).to_string())
                .unwrap_or_else(|| primary.arch.clone());

            let out_deps = if out_table.contains_key("dependencies") {
                table_strings(out_table, "dependencies").iter().map(|s| parse_dep(s)).collect()
            } else {
                Vec::new()
            };
            let out_provides = if out_table.contains_key("provides") {
                table_strings(out_table, "provides")
            } else {
                Vec::new()
            };
            let out_conffiles = if out_table.contains_key("conffiles") {
                table_strings(out_table, "conffiles")
            } else {
                Vec::new()
            };

            let r = Recipe {
                name: out_name.clone(),
                version: out_ver,
                release: out_rel,
                description: out_desc,
                license: out_lic,
                channel: out_channel,
                arch: out_arch,
                source_url: primary.source_url.clone(),
                source_sha256: primary.source_sha256.clone(),
                upstream_url: primary.upstream_url.clone(),
                upstream_version_regex: primary.upstream_version_regex.clone(),
                dependencies: out_deps,
                build_dependencies: primary.build_dependencies.clone(),
                provides: out_provides,
                conffiles: out_conffiles,
            };

            if out_name == primary.name {
                primary_updated = true;
            }
            result.push(r);
        }

        if !primary_updated {
            result.insert(0, primary);
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_source_table() {
        let r = Recipe::from_toml(
            "[package]\n\
             name = \"os-release\"\n\
             version = \"1\"\n\
             release = \"1\"\n\
             \n\
             [source]\n\
             url = \"https://example.com/a.tar.gz\"\n\
             sha256 = \"777\"\n",
        )
        .unwrap();
        assert_eq!(r.source_url, "https://example.com/a.tar.gz");
        assert_eq!(r.source_sha256, "777");
    }

    #[test]
    fn arch_canonicalization() {
        let r = Recipe::from_toml(
            "[package]\n\
             name = \"zlib\"\n\
             version = \"1.3.2\"\n\
             release = \"2\"\n\
             arch = \"x86_64\"\n",
        )
        .unwrap();
        assert_eq!(r.arch, "amd64");
        let r = Recipe::from_toml(
            "[package]\n\
             name = \"shc\"\n\
             version = \"1.0.0\"\n\
             release = \"1\"\n\
             arch = \"any\"\n",
        )
        .unwrap();
        assert_eq!(r.arch, "any");
    }

    #[test]
    fn multi_source_array_primary_element_wins() {
        // bash-style [[source]] arrays: patches follow the tarball, so the
        // detail page must show element zero -- the archive sage unpacks.
        let r = Recipe::from_toml(
            "name = \"bash\"\n\
             version = \"5.3\"\n\
             release = \"1\"\n\
             \n\
             [[source]]\n\
             url = \"https://ftpmirror.gnu.org/gnu/bash/bash-5.3.tar.gz\"\n\
             sha256 = \"aaa\"\n\
             \n\
             [[source]]\n\
             url = \"https://ftpmirror.gnu.org/gnu/bash/bash-5.3-patch01\"\n\
             sha256 = \"bbb\"\n",
        )
        .unwrap();
        assert_eq!(
            r.source_url,
            "https://ftpmirror.gnu.org/gnu/bash/bash-5.3.tar.gz"
        );
        assert_eq!(r.source_sha256, "aaa");
    }

    #[test]
    fn v2_upstream_table_and_issue_compatibility_spelling() {
        let preferred = Recipe::from_toml(
            "schema_version = 2\n\
             [package]\nname = \"zlib\"\nversion = \"1.3.2\"\n\
             [upstream]\nurl = \"https://zlib.net/\"\nversion_regex = 'zlib-(\\d+\\.\\d+\\.\\d+)'\n\
             [build]\nsystem = \"cmake\"\n\
             [build.toolchain.compiler]\nfamily = \"clang\"\npackage = \"clang\"\nminimum_version = \"22\"\n\
             [build.toolchain.linker]\nfamily = \"lld\"\npackage = \"lld\"\nminimum_version = \"22\"\n",
        )
        .unwrap();
        assert_eq!(preferred.upstream_url, "https://zlib.net/");
        assert_eq!(preferred.upstream_version_regex, r"zlib-(\d+\.\d+\.\d+)");
        assert!(preferred
            .build_dependencies
            .iter()
            .any(|d| d.name == "clang" && d.req == ">= 22"));
        assert!(preferred
            .build_dependencies
            .iter()
            .any(|d| d.name == "lld" && d.req == ">= 22"));

        let cargo = Recipe::from_toml(
            "schema_version = 2\n\
             [package]\nname = \"cargo-example\"\nversion = \"1\"\n\
             [build]\nsystem = \"cargo\"\n\
             [build.toolchain.rust]\nfamily = \"rustc\"\npackage = \"rust\"\nminimum_version = \"1.90\"\n",
        )
        .unwrap();
        assert!(cargo
            .build_dependencies
            .iter()
            .any(|d| d.name == "rust" && d.req == ">= 1.90"));

        let compatible = Recipe::from_toml(
            "[package]\nname = \"zlib\"\nversion = \"1.3.2\"\n\
             upstream = \"https://zlib.net/\"\nupstream_regex = 'v(\\d+)'\n",
        )
        .unwrap();
        assert_eq!(compatible.upstream_url, "https://zlib.net/");
        assert_eq!(compatible.upstream_version_regex, r"v(\d+)");
    }

    #[test]
    fn multi_output_recipe_yields_all_subpackages() {
        let text = r#"
schema_version = 2
[package]
name = "zlib"
version = "1.3.2"
release = "1"
description = "zlib runtime"
license = "Zlib"
channel = "system"
arch = "amd64"

[source]
url = "https://zlib.net/zlib-1.3.2.tar.gz"
sha256 = "bb329a0a2cd0274d05519d61c667c062e06990d72e125ee2dfa8de64f0119d16"

[build]
system = "autotools"
payload = "outputs"

[[build.outputs]]
name = "zlib"
description = "zlib runtime"
license = "Zlib"
provides = ["zlib", "so:libz.so.1"]

[[build.outputs]]
name = "zlib-dev"
description = "zlib development headers"
license = "Zlib"
dependencies = ["zlib >= 1.3.2"]
provides = ["zlib-dev"]
"#;
        let all = Recipe::from_toml_all(text).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "zlib");
        assert_eq!(all[0].provides, vec!["zlib", "so:libz.so.1"]);
        assert_eq!(all[1].name, "zlib-dev");
        assert_eq!(all[1].dependencies.len(), 1);
        assert_eq!(all[1].dependencies[0].name, "zlib");
        assert_eq!(all[1].dependencies[0].req, ">= 1.3.2");
    }
}

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

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
    pub check_dependencies: Vec<Dep>,
    pub provides: Vec<String>,
    pub conffiles: Vec<String>,
}

/// Canonical architecture spelling used by recipe paths and published indexes.
pub fn canonical_arch(arch: &str) -> &str {
    match arch.trim() {
        "x86_64" => "amd64",
        "arm64" => "aarch64",
        "arm" | "armhf" | "armv7" | "armv7l" => "armv7",
        "riscv64" => "riscv64",
        other => other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
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
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalized_sha256(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn basename(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && Path::new(value).file_name().and_then(|name| name.to_str()) == Some(value)
}

/// Validate the v2 portions whose meaning must stay identical to Sage. The
/// web cache is not a second parser: it rejects a recipe that Sage would
/// reject, including check-phase and structured-patch semantics.
fn validate_v2(doc: &toml::Table) -> Result<()> {
    let schema = doc
        .get("schema_version")
        .and_then(|value| value.as_integer())
        .unwrap_or(1);
    if schema != 2 {
        return Ok(());
    }

    let package = doc
        .get("package")
        .and_then(|value| value.as_table())
        .context("recipe has no [package] table")?;
    let package_arch = package
        .get("arch")
        .and_then(|value| value.as_str())
        .context("package.arch is required for recipe v2")?;
    anyhow::ensure!(
        matches!(
            canonical_arch(package_arch),
            "amd64" | "aarch64" | "riscv64" | "armv7" | "any"
        ),
        "unsupported package.arch '{package_arch}'"
    );
    for key in [
        "dependencies",
        "conflicts",
        "build_dependencies",
        "check_dependencies",
        "provides",
        "conffiles",
    ] {
        if let Some(value) = package.get(key) {
            let Some(items) = value.as_array() else {
                anyhow::bail!("package.{key} must be an array of strings");
            };
            if items
                .iter()
                .any(|item| item.as_str().is_none_or(str::is_empty))
            {
                anyhow::bail!("package.{key} must contain non-empty strings");
            }
        }
    }
    let check_dependencies = package
        .get("check_dependencies")
        .and_then(|value| value.as_array())
        .map(|items| !items.is_empty())
        .unwrap_or(false);

    let build = doc
        .get("build")
        .and_then(|value| value.as_table())
        .context("recipe v2 has no [build] table")?;
    let system = build
        .get("system")
        .and_then(|value| value.as_str())
        .context("recipe v2 build.system is required")?;
    let payload = build
        .get("payload")
        .and_then(|value| value.as_str())
        .context("recipe v2 build.payload is required")?;
    anyhow::ensure!(
        matches!(
            system,
            "autotools" | "cmake" | "meson" | "xmake" | "cargo" | "make" | "script"
        ),
        "unsupported v2 build.system '{system}'"
    );
    anyhow::ensure!(
        matches!(payload, "all" | "allowlist" | "outputs"),
        "unsupported v2 build.payload '{payload}'"
    );

    let mut has_check_phase = false;
    if let Some(value) = build.get("steps") {
        let Some(steps) = value.as_array() else {
            anyhow::bail!("build.steps must be an array");
        };
        let mut names = HashSet::new();
        for item in steps {
            let Some(step) = item.as_table() else {
                anyhow::bail!("build.steps entries must be tables");
            };
            for key in step.keys() {
                anyhow::ensure!(
                    matches!(
                        key.as_str(),
                        "name" | "phase" | "cwd" | "command" | "unsafe_shell"
                    ),
                    "unknown build.steps key '{key}'"
                );
            }
            for key in ["name", "phase", "command"] {
                if step
                    .get(key)
                    .and_then(|value| value.as_str())
                    .is_none_or(str::is_empty)
                {
                    anyhow::bail!("build.steps entries require non-empty {key}");
                }
            }
            let name = step["name"].as_str().unwrap();
            let phase = step["phase"].as_str().unwrap();
            anyhow::ensure!(names.insert(name), "duplicate build.steps name '{name}'");
            anyhow::ensure!(
                matches!(
                    phase,
                    "prepare"
                        | "pre-build"
                        | "post-build"
                        | "check"
                        | "pre-install"
                        | "install"
                        | "post-install"
                ),
                "unsupported build.steps phase '{phase}'"
            );
            let cwd = step
                .get("cwd")
                .and_then(|value| value.as_str())
                .unwrap_or("source");
            anyhow::ensure!(
                matches!(cwd, "source" | "build" | "package"),
                "unsupported build.steps cwd '{cwd}'"
            );
            if let Some(value) = step.get("unsafe_shell") {
                anyhow::ensure!(
                    value.as_bool().is_some(),
                    "build.steps unsafe_shell must be boolean"
                );
            }
            has_check_phase |= phase == "check";
        }
    }
    if check_dependencies {
        anyhow::ensure!(
            has_check_phase,
            "package.check_dependencies require a build.steps phase='check'"
        );
    }

    let source_filename = |url: &str| {
        let path = url.split(['?', '#']).next().unwrap_or_default();
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string()
    };
    let mut source_hashes = BTreeMap::new();
    if let Some(value) = doc.get("source") {
        let sources: Vec<&toml::Table> = match value {
            toml::Value::Table(table) => vec![table],
            toml::Value::Array(items) => {
                let mut tables = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    tables.push(
                        item.as_table()
                            .with_context(|| format!("source[{index}] must be a table"))?,
                    );
                }
                tables
            }
            _ => anyhow::bail!("recipe.source must be a table or array of tables"),
        };
        for (index, source) in sources.iter().enumerate() {
            let url = source
                .get("url")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .with_context(|| format!("source[{index}] requires a non-empty url"))?;
            let sha = source
                .get("sha256")
                .and_then(|value| value.as_str())
                .filter(|value| valid_sha256(value))
                .with_context(|| format!("source[{index}] requires a 64-hex sha256"))?;
            let name = source_filename(url);
            anyhow::ensure!(basename(&name), "source[{index}] URL must have a basename");
            anyhow::ensure!(
                source_hashes
                    .insert(name.clone(), normalized_sha256(sha))
                    .is_none(),
                "source URLs must have unique filenames: {name}"
            );
        }
    }

    let global_strip = match build.get("patch_strip") {
        None => 1,
        Some(value) => value
            .as_integer()
            .context("build.patch_strip must be an integer")?,
    };
    anyhow::ensure!(
        (0..=9).contains(&global_strip),
        "build.patch_strip must be between 0 and 9"
    );
    let mut patch_names = HashSet::new();
    let mut patch_hashes: BTreeMap<String, String> = BTreeMap::new();
    if let Some(value) = build.get("patches") {
        let Some(patches) = value.as_array() else {
            anyhow::bail!("build.patches must be an array");
        };
        for (index, item) in patches.iter().enumerate() {
            let (file, strip, explicit_hash) = if let Some(file) = item.as_str() {
                (file.to_string(), global_strip, None)
            } else {
                let table = item
                    .as_table()
                    .with_context(|| format!("build.patches[{index}] must be a string or table"))?;
                for key in table.keys() {
                    anyhow::ensure!(
                        matches!(key.as_str(), "file" | "strip" | "sha256"),
                        "unknown structured patch key '{key}'"
                    );
                }
                let file = table
                    .get("file")
                    .and_then(|value| value.as_str())
                    .context("structured patch requires file")?
                    .to_string();
                let strip = match table.get("strip") {
                    None => global_strip,
                    Some(value) => value.as_integer().with_context(|| {
                        format!("build.patches[{index}].strip must be an integer")
                    })?,
                };
                anyhow::ensure!(
                    (0..=9).contains(&strip),
                    "build.patches[{index}].strip must be between 0 and 9"
                );
                let hash = table
                    .get("sha256")
                    .and_then(|value| value.as_str())
                    .filter(|value| valid_sha256(value))
                    .context("structured patch requires a 64-hex sha256")?
                    .to_string();
                (file, strip, Some(hash))
            };
            let _ = strip;
            anyhow::ensure!(
                basename(&file),
                "build.patches entries require a basename file"
            );
            anyhow::ensure!(
                patch_names.insert(file.clone()),
                "build.patches cannot declare the same file more than once: {file}"
            );
            if let Some(hash) = explicit_hash {
                patch_hashes.insert(file, normalized_sha256(&hash));
            } else {
                patch_hashes.insert(file, String::new());
            }
        }
    }

    if let Some(value) = build.get("patch_checksums") {
        let Some(checksums) = value.as_table() else {
            anyhow::bail!("build.patch_checksums must be a table");
        };
        for (file, value) in checksums {
            let hash = value
                .as_str()
                .filter(|value| valid_sha256(value))
                .with_context(|| format!("patch checksum '{file}' must be a 64-hex SHA-256"))?;
            anyhow::ensure!(basename(file), "patch checksum key must be a basename");
            anyhow::ensure!(
                patch_names.contains(file),
                "patch checksum names an undeclared patch: {file}"
            );
            if let Some(previous) = patch_hashes.get_mut(file) {
                let hash = normalized_sha256(hash);
                anyhow::ensure!(
                    previous.is_empty() || *previous == hash,
                    "patch '{file}' has conflicting sha256 declarations"
                );
                *previous = hash;
            }
        }
    }
    for (file, hash) in patch_hashes {
        let source_hash = source_hashes.get(&file);
        if let Some(source_hash) = source_hash {
            anyhow::ensure!(
                hash.is_empty() || hash == *source_hash,
                "patch '{file}' sha256 conflicts with its source declaration"
            );
        }
        anyhow::ensure!(
            !hash.is_empty() || source_hash.is_some(),
            "every build.patches entry requires a SHA-256 declaration"
        );
    }
    Ok(())
}

impl Recipe {
    pub fn from_toml(text: &str) -> Result<Self> {
        let doc: toml::Table = toml::from_str(text).context("recipe.toml is not valid TOML")?;
        validate_v2(&doc)?;
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
        let mut check_dependencies = table_strings(&doc, "check_dependencies");
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
            check_dependencies.extend(table_strings(scope, "check_dependencies"));
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
            check_dependencies: check_dependencies.iter().map(|s| parse_dep(s)).collect(),
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
            let Some(out_table) = item.as_table() else {
                continue;
            };
            let out_name = table_str(out_table, "name");
            let Some(out_name) = out_name else {
                continue;
            };

            let out_ver =
                table_str(out_table, "version").unwrap_or_else(|| primary.version.clone());
            let out_rel =
                table_str(out_table, "release").unwrap_or_else(|| primary.release.clone());
            let out_desc =
                table_str(out_table, "description").unwrap_or_else(|| primary.description.clone());
            let out_lic =
                table_str(out_table, "license").unwrap_or_else(|| primary.license.clone());
            let out_channel =
                table_str(out_table, "channel").unwrap_or_else(|| primary.channel.clone());
            let out_arch = table_str(out_table, "arch")
                .map(|a| canonical_arch(&a).to_string())
                .unwrap_or_else(|| primary.arch.clone());

            let out_deps = if out_table.contains_key("dependencies") {
                table_strings(out_table, "dependencies")
                    .iter()
                    .map(|s| parse_dep(s))
                    .collect()
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
                check_dependencies: primary.check_dependencies.clone(),
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
             release = \"1\"\narch = \"amd64\"\n\
             check_dependencies = [\"pkg-config >= 0.29\"]\n\
             [upstream]\nurl = \"https://zlib.net/\"\nversion_regex = 'zlib-(\\d+\\.\\d+\\.\\d+)'\n\
             [build]\nsystem = \"cmake\"\npayload = \"all\"\n\
             [[build.steps]]\nname = \"check\"\nphase = \"check\"\ncommand = \"ctest --test-dir build\"\n\
             [build.toolchain.compiler]\nfamily = \"clang\"\npackage = \"clang\"\nminimum_version = \"22\"\n\
             [build.toolchain.linker]\nfamily = \"lld\"\npackage = \"lld\"\nminimum_version = \"22\"\n",
        )
        .unwrap();
        assert_eq!(preferred.upstream_url, "https://zlib.net/");
        assert_eq!(preferred.upstream_version_regex, r"zlib-(\d+\.\d+\.\d+)");
        assert_eq!(
            preferred.check_dependencies,
            vec![Dep {
                name: "pkg-config".into(),
                req: ">= 0.29".into(),
            }]
        );
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
             arch = \"amd64\"\n\
             [build]\nsystem = \"cargo\"\npayload = \"all\"\n\
             [build.toolchain.rust]\nfamily = \"rustc\"\npackage = \"rust\"\nminimum_version = \"1.90\"\n",
        )
        .unwrap();
        assert!(cargo
            .build_dependencies
            .iter()
            .any(|d| d.name == "rust" && d.req == ">= 1.90"));

        let compatible = Recipe::from_toml(
            "[package]\nname = \"zlib\"\nversion = \"1.3.2\"\n\
             arch = \"amd64\"\n\
             upstream = \"https://zlib.net/\"\nupstream_regex = 'v(\\d+)'\n",
        )
        .unwrap();
        assert_eq!(compatible.upstream_url, "https://zlib.net/");
        assert_eq!(compatible.upstream_version_regex, r"v(\d+)");
    }
    #[test]
    fn v2_structured_patch_hash_is_self_contained() {
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let text = format!(
            "schema_version = 2\n\
             [package]\nname = \"patched\"\nversion = \"1\"\nrelease = \"1\"\narch = \"amd64\"\n\
             [source]\nurl = \"https://example.invalid/fix.patch\"\nsha256 = \"{hash}\"\n\
             [build]\nsystem = \"script\"\npayload = \"allowlist\"\n\
             install_files = [\"usr/share/patched/**\"]\n\
             patches = [{{ file = \"fix.patch\", strip = 1, sha256 = \"{hash}\" }}]\n\
             [[build.steps]]\nname = \"install\"\nphase = \"install\"\ncommand = \"true\"\n"
        );
        let parsed = Recipe::from_toml(&text).unwrap();
        assert_eq!(parsed.check_dependencies, Vec::<Dep>::new());
        assert!(Recipe::from_toml(&text.replacen(hash, &"b".repeat(64), 1)).is_err());
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

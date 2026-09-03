//! Presentation adapters around Sage 0.4's canonical domain types.
//!
//! Package parsing deliberately does not live here. `sage-build::RecipeSpec`
//! validates recipes and `sage-archive::inspect_package` validates archives;
//! this module only keeps the web view's compact dependency representation.

use serde::{Deserialize, Serialize};

/// Canonical architecture spelling shared by recipe paths, manifests, and
/// status comparisons.
pub fn canonical_arch(arch: &str) -> &str {
    match arch.trim() {
        "x86_64" => "amd64",
        "arm64" => "aarch64",
        "arm" | "armhf" | "armv7" | "armv7l" => "armv7",
        other => other,
    }
}

/// Dependency text shown by the presentation layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dep {
    pub name: String,
    pub req: String,
}

impl From<&sage_core::Dependency> for Dep {
    fn from(dependency: &sage_core::Dependency) -> Self {
        let mut req = String::new();
        if dependency.op != sage_core::ConstraintOp::Any {
            let op = dependency.op;
            req.push_str(&op.to_string());
            if let Some(version) = &dependency.version {
                req.push(' ');
                req.push_str(&version.to_string());
            }
        }
        Self {
            name: dependency.name.clone(),
            req,
        }
    }
}

pub fn parse_dep(raw: &str) -> Dep {
    raw.parse::<sage_core::Dependency>()
        .map(|dependency| Dep::from(&dependency))
        .unwrap_or_else(|_| Dep {
            name: raw.to_string(),
            req: String::new(),
        })
}

use serde::Serialize;

/// Build state of one recipe, derived on demand from the
/// packages-vs-published diff. Never stored: the two tables are the truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// No published record -- awaiting first build.
    Missing,
    /// Repo version older than the recipe -- awaiting rebuild/re-upload.
    Outdated,
    /// Recipe and repo agree.
    Built,
    /// Repo newer than the recipe -- recipe lags, needs attention.
    Ahead,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Missing => "missing",
            State::Outdated => "outdated",
            State::Built => "built",
            State::Ahead => "ahead",
        }
    }
}

/// Compare versions with Sage 0.4's version algebra. Bare upstream versions
/// are accepted for the presentation layer and receive release zero.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::str::FromStr;
    let parse = |value: &str| {
        sage_core::Version::from_str(value).unwrap_or_else(|_| sage_core::Version::new(0, value, 0))
    };
    parse(a).cmp(&parse(b))
}

pub fn derive(
    recipe_version: &str,
    recipe_release: &str,
    published: Option<(&str, &str)>,
) -> State {
    derive_with_epoch(
        0,
        recipe_version,
        recipe_release,
        published.map(|(version, release)| (0, version, release)),
    )
}

/// Sage's epoch is part of the version coordinate and must win over any
/// upstream/release comparison.
pub fn derive_with_epoch(
    recipe_epoch: u32,
    recipe_version: &str,
    recipe_release: &str,
    published: Option<(u32, &str, &str)>,
) -> State {
    let Some((pub_epoch, pub_ver, pub_rel)) = published else {
        return State::Missing;
    };
    match sage_core::Version::new(pub_epoch, pub_ver, pub_rel.parse().unwrap_or(0)).cmp(
        &sage_core::Version::new(
            recipe_epoch,
            recipe_version,
            recipe_release.parse().unwrap_or(0),
        ),
    ) {
        std::cmp::Ordering::Less => State::Outdated,
        std::cmp::Ordering::Equal => State::Built,
        std::cmp::Ordering::Greater => State::Ahead,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_ordering() {
        assert_eq!(compare_versions("1.10", "1.9"), std::cmp::Ordering::Greater);
        assert_eq!(
            compare_versions("1.2.3", "1.2.3"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_versions("22.1.8-2", "22.1.8-10"),
            std::cmp::Ordering::Less
        );
        assert_eq!(compare_versions("2.44", "2.9"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn states() {
        assert_eq!(derive("1.0", "1", None), State::Missing);
        assert_eq!(derive("2.0", "1", Some(("1.9", "4"))), State::Outdated);
        assert_eq!(derive("1.0", "1", Some(("1.0", "1"))), State::Built);
        assert_eq!(derive("1.0", "1", Some(("1.1", "1"))), State::Ahead);
    }
}

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

/// Segment-wise comparison the way sage orders versions: digit-led chunks
/// compare numerically, everything else lexically. `"1.10" > "1.9"`,
/// `"22.1.8-3" > "22.1.8-2"`.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut ai = a.split(['.', '-']);
    let mut bi = b.split(['.', '-']);
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (numeric(x), numeric(y)) {
                    (Some(nx), Some(ny)) => nx.cmp(&ny),
                    _ => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

fn numeric(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

pub fn derive(recipe_version: &str, recipe_release: &str, published: Option<(&str, &str)>) -> State {
    let Some((pub_ver, pub_rel)) = published else {
        return State::Missing;
    };
    match compare_versions(
        &format!("{pub_ver}-{pub_rel}"),
        &format!("{recipe_version}-{recipe_release}"),
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
        assert_eq!(compare_versions("1.2.3", "1.2.3"), std::cmp::Ordering::Equal);
        assert_eq!(compare_versions("22.1.8-2", "22.1.8-10"), std::cmp::Ordering::Less);
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

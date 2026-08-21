//! Global dependency graph: layered (longest-path) layout over the recipe
//! dependency edges with a barycenter pass to tame edge crossings, emitted as
//! an inline SVG. Node color carries the derived build state so the picture
//! doubles as a build-coverage map.

use std::collections::HashMap;

use crate::db::PackageRow;
use crate::status::{self, State as BuildState};

const NODE_H: f32 = 22.0;
const NODE_VGAP: f32 = 7.0;
const LEVEL_GAP: f32 = 56.0; // horizontal distance between levels
const CHAR_W: f32 = 6.4;
const NODE_PAD: f32 = 14.0;
const EDGE_STROKE: &str = "#c9c5bc";

pub struct GraphNode {
    pub name: String,
    pub level: usize,
    pub state: BuildState,
    x: f32,
    y: f32,
    w: f32,
}

pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(usize, usize)>, // (dep, dependent)
    pub width: f32,
    pub height: f32,
}

/// Build the layered graph. `published` maps package name → repo-side
/// (version, release) for build-state coloring.
pub fn build(
    rows: &[PackageRow],
    published: &HashMap<String, (String, String)>,
) -> Graph {
    let index: HashMap<&str, usize> =
        rows.iter().enumerate().map(|(i, r)| (r.name.as_str(), i)).collect();
    let provides_to: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .flat_map(|(i, r)| r.provides.iter().map(move |p| (p.as_str(), i)))
        .collect();

    // Resolve dependency names onto node indices (name match first, then
    // provides); drop self-edges and unresolved names.
    let resolved: Vec<Vec<usize>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            r.dependencies
                .iter()
                .filter_map(|d| {
                    index
                        .get(d.name.as_str())
                        .copied()
                        .or_else(|| provides_to.get(d.name.as_str()).copied())
                        .filter(|&to| to != i)
                })
                .collect()
        })
        .collect();
    let edges: Vec<(usize, usize)> = resolved
        .iter()
        .enumerate()
        .flat_map(|(me, deps)| deps.iter().map(move |&d| (d, me)))
        .collect();

    let levels = longest_path_levels(&resolved);
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| (levels[i], rows[i].name.as_str()));
    barycenter_passes(&mut order, &levels, &edges, rows.len(), 4);

    let mut level_counts: Vec<usize> = Vec::new();
    for &i in &order {
        if level_counts.len() <= levels[i] {
            level_counts.resize(levels[i] + 1, 0);
        }
        level_counts[levels[i]] += 1;
    }
    let height = level_counts
        .iter()
        .map(|&n| n as f32 * (NODE_H + NODE_VGAP))
        .fold(0.0f32, f32::max);

    let mut slots = vec![0usize; rows.len()];
    let nodes: Vec<GraphNode> = order
        .iter()
        .map(|&i| {
            let lv = levels[i];
            let slot = slots[lv];
            slots[lv] += 1;
            GraphNode {
                name: rows[i].name.clone(),
                level: lv,
                state: status::derive(
                    &rows[i].version,
                    &rows[i].release,
                    published.get(&rows[i].name).map(|(v, rl)| (v.as_str(), rl.as_str())),
                ),
                x: lv as f32 * LEVEL_GAP,
                y: slot as f32 * (NODE_H + NODE_VGAP),
                w: (rows[i].name.len() as f32 * CHAR_W + NODE_PAD).max(56.0),
            }
        })
        .collect();

    let width = levels.iter().copied().max().unwrap_or(0) as f32 * LEVEL_GAP + 150.0;
    Graph { nodes, edges, width, height }
}

impl Graph {
    /// Inline SVG for embedding in the /graph page. Node classes reuse the
    /// page's badge palette; every node links to its detail page.
    pub fn render_svg(&self) -> String {
        let mut out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {:.0} {:.0}\" \
             font-family=\"ui-monospace,Menlo,Consolas,monospace\" font-size=\"11\">",
            self.width, self.height
        );
        for &(dep, me) in &self.edges {
            let (a, b) = (&self.nodes[dep], &self.nodes[me]);
            let x1 = a.x + a.w;
            let y1 = a.y + NODE_H / 2.0;
            let x2 = b.x;
            let y2 = b.y + NODE_H / 2.0;
            let mid = (x2 - x1) / 2.0;
            out.push_str(&format!(
                "<path d=\"M{x1:.0},{y1:.0} C{:.0},{y1:.0} {:.0},{y2:.0} {x2:.0},{y2:.0}\" \
                 fill=\"none\" stroke=\"{EDGE_STROKE}\" stroke-width=\"1\"/>",
                x1 + mid,
                x2 - mid
            ));
        }
        for n in &self.nodes {
            let h = format!("{NODE_H:.0}");
            out.push_str(&format!(
                "<a href=\"/package/{}\" target=\"_top\"><title>{} · {}</title>\
                 <rect class=\"st-{}\" x=\"{:.0}\" y=\"{:.0}\" width=\"{:.0}\" height=\"{h}\" rx=\"4\"/></a>",
                n.name, n.name, n.state.label(), n.state.label(), n.x, n.y, n.w
            ));
            let ty = n.y + NODE_H / 2.0 + 3.5;
            out.push_str(&format!(
                "<text x=\"{:.0}\" y=\"{ty:.0}\" fill=\"#ffffff\" \
                 style=\"pointer-events:none\">{}</text>",
                n.x + n.w / 2.0,
                xml_escape(&n.name)
            ));
        }
        out.push_str("</svg>");
        out
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn longest_path_levels(resolved: &[Vec<usize>]) -> Vec<usize> {
    // memo[i] = longest dep-chain ending at i; cycle back-edges are ignored.
    fn walk(i: usize, resolved: &[Vec<usize>], memo: &mut [Option<usize>]) -> usize {
        if let Some(l) = memo[i] {
            return l;
        }
        memo[i] = Some(0); // cycle guard: in-progress nodes read as level 0
        let level = resolved[i]
            .iter()
            .map(|&d| walk(d, resolved, memo) + 1)
            .max()
            .unwrap_or(0);
        memo[i] = Some(level);
        level
    }
    let mut memo = vec![None; resolved.len()];
    (0..resolved.len()).map(|i| walk(i, resolved, &mut memo)).collect()
}

/// Reorder within levels toward each node's neighbors' average row position,
/// alternating sweep direction — cheap crossing reduction.
fn barycenter_passes(
    order: &mut [usize],
    levels: &[usize],
    edges: &[(usize, usize)],
    n: usize,
    passes: usize,
) {
    let mut neighbors: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        neighbors[a].push(b);
        neighbors[b].push(a);
    }
    let pos_of = |order: &[usize]| -> Vec<usize> {
        let mut p = vec![0usize; n];
        for (s, &nd) in order.iter().enumerate() {
            p[nd] = s;
        }
        p
    };
    for pass in 0..passes {
        let range: Vec<usize> = match pass % 2 {
            0 => (0..order.len()).collect(),
            _ => (0..order.len()).rev().collect(),
        };
        for slot in range {
            let node = order[slot];
            if neighbors[node].is_empty() {
                continue;
            }
            let pos = pos_of(order);
            let avg = neighbors[node].iter().map(|&nb| pos[nb] as f64).sum::<f64>()
                / neighbors[node].len() as f64;
            // Bubble toward that average within the same level.
            let mut s = slot;
            while s > 0
                && levels[order[s - 1]] == levels[order[s]]
                && (pos[order[s - 1]] as f64) > avg
            {
                order.swap(s - 1, s);
                s -= 1;
            }
            while s + 1 < order.len()
                && levels[order[s + 1]] == levels[order[s]]
                && (pos[order[s + 1]] as f64) < avg
            {
                order.swap(s + 1, s);
                s += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Dep;

    fn row(name: &str, deps: &[&str]) -> PackageRow {
        PackageRow {
            name: name.into(),
            category: "misc".into(),
            version: "1.0".into(),
            release: "1".into(),
            description: String::new(),
            license: String::new(),
            channel: "system".into(),
            provides: Vec::new(),
            dependencies: deps.iter().map(|d| Dep { name: d.to_string(), req: String::new() }).collect(),
            build_dependencies: Vec::new(),
            conffiles: Vec::new(),
            source_url: String::new(),
            source_sha256: String::new(),
            recipe_path: String::new(),
        }
    }

    #[test]
    fn layers_and_cycles() {
        // a ← c ← d chain; b depends only on itself (self-edge dropped → level 0)
        let rows = vec![row("a", &[]), row("c", &["a"]), row("d", &["c"]), row("b", &["b"])];
        let g = build(&rows, &HashMap::new());
        let lv = |name: &str| g.nodes.iter().find(|n| n.name == name).unwrap().level;
        assert_eq!(lv("a"), 0);
        assert_eq!(lv("c"), 1);
        assert_eq!(lv("d"), 2);
        assert_eq!(lv("b"), 0);
        assert_eq!(g.edges.len(), 2);
    }

    #[test]
    fn svg_has_links_and_classes() {
        let rows = vec![row("a", &[]), row("b", &["a"])];
        let g = build(&rows, &HashMap::new());
        let svg = g.render_svg();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("href=\"/package/a\""));
        assert!(svg.contains("st-missing"));
        assert!(svg.contains("</svg>"));
    }

    #[test]
    fn provides_resolve() {
        let mut r = row("consumer", &["cc"]);
        r.provides = Vec::new();
        let mut provider = row("gcc", &[]);
        provider.provides = vec!["cc".into()];
        let g = build(&[provider, r], &HashMap::new());
        assert_eq!(g.edges.len(), 1);
    }
}

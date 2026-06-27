//! Rendering of the issue dependency forest as indented ASCII lines.

use std::collections::HashSet;

use super::ListedIssue;

/// Maximum dependency-chain depth [`dependency_tree`] descends before stopping.
///
/// `dependson` comes from issue markdown files (hand- or externally-written), so
/// a pathological single chain `#1 ← #2 ← … ← #N` makes [`walk_children`] recurse
/// N deep — deep enough to overflow the stack and abort the process. The
/// `visited` set stops *re-expanding* a node (cycles/diamonds) but does not bound
/// a fresh linear chain's depth, so this hard cap does. It is far beyond any real
/// dependency tree; hitting it means the data is pathological, and the tree is
/// truncated with a marker rather than crashing.
const MAX_DEPTH: usize = 256;

/// Render a dependency forest as indented ASCII lines: each issue appears under
/// the issues it `dependson`, so reading top-to-bottom follows the order work
/// can be picked up. Roots are issues with no dependencies; issues reached again
/// (diamonds or cycles) are shown once with a `↑` marker and not re-expanded.
pub fn dependency_tree(items: &[ListedIssue]) -> Vec<String> {
    use std::collections::BTreeMap;

    let by_number: BTreeMap<u32, &ListedIssue> =
        items.iter().map(|i| (i.summary.number, i)).collect();
    // children[d] = issues that depend on d, kept sorted by number.
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for item in items {
        for dep in &item.summary.dependson {
            children.entry(*dep).or_default().push(item.summary.number);
        }
    }

    let mut visited: HashSet<u32> = HashSet::new();
    let mut out = Vec::new();

    // Start from: dependency targets that don't exist as issues (so their
    // dependents are still shown), then roots (no dependencies), then every
    // remaining node so nothing is dropped amid orphaned deps or cycles.
    let mut starts: Vec<u32> = children
        .keys()
        .copied()
        .filter(|d| !by_number.contains_key(d))
        .collect();
    starts.extend(
        items
            .iter()
            .filter(|i| i.summary.dependson.is_empty())
            .map(|i| i.summary.number),
    );
    starts.extend(items.iter().map(|i| i.summary.number));

    for num in starts {
        if visited.contains(&num) {
            continue;
        }
        out.push(node_label(num, &by_number, &mut visited));
        walk_children(num, &children, &by_number, "", 0, &mut visited, &mut out);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn walk_children(
    num: u32,
    children: &std::collections::BTreeMap<u32, Vec<u32>>,
    by_number: &std::collections::BTreeMap<u32, &ListedIssue>,
    prefix: &str,
    depth: usize,
    visited: &mut HashSet<u32>,
    out: &mut Vec<String>,
) {
    let Some(kids) = children.get(&num) else {
        return;
    };
    // Stop before a pathologically deep dependency chain overflows the stack
    // (see MAX_DEPTH). Mark the cutoff so the truncation is visible rather than a
    // subtree silently vanishing.
    if depth >= MAX_DEPTH {
        out.push(format!("{prefix}… (depth limit reached)"));
        return;
    }
    let last_index = kids.len() - 1;
    for (i, &child) in kids.iter().enumerate() {
        let is_last = i == last_index;
        let branch = if is_last { "└─ " } else { "├─ " };
        let already = visited.contains(&child);
        out.push(format!(
            "{prefix}{branch}{}",
            node_label(child, by_number, visited)
        ));
        if !already {
            let extension = if is_last { "   " } else { "│  " };
            walk_children(
                child,
                children,
                by_number,
                &format!("{prefix}{extension}"),
                depth + 1,
                visited,
                out,
            );
        }
    }
}

/// One node's label, marking the first/repeat visit. Records the visit.
fn node_label(
    num: u32,
    by_number: &std::collections::BTreeMap<u32, &ListedIssue>,
    visited: &mut HashSet<u32>,
) -> String {
    let repeat = !visited.insert(num);
    match by_number.get(&num) {
        Some(item) => {
            let mark = if repeat { " ↑" } else { "" };
            format!(
                "#{} {} [{}]{mark}",
                item.summary.number, item.summary.title, item.summary.status
            )
        }
        // A dependency that points at a non-existent issue.
        None => format!("#{num} (missing)"),
    }
}

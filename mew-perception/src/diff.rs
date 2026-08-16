use crate::TreeNode;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct TreeDiff {
    pub added: Vec<TreeNode>,
    pub removed: Vec<TreeNode>,
    pub changed: Vec<ChangedNode>,
}

#[derive(Debug, Clone)]
pub struct ChangedNode {
    pub node: TreeNode,
    pub old_role: String,
    pub old_name: String,
    pub old_value: String,
}

impl TreeDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn serialize_compact(&self) -> String {
        let mut out = String::new();
        if !self.added.is_empty() {
            out.push_str("[Added]\n");
            for node in &self.added {
                out.push_str(&format!("  {}\n", format_node(node)));
            }
        }
        if !self.removed.is_empty() {
            out.push_str("[Removed]\n");
            for node in &self.removed {
                out.push_str(&format!("  {}\n", format_node(node)));
            }
        }
        if !self.changed.is_empty() {
            out.push_str("[Changed]\n");
            for change in &self.changed {
                let node = &change.node;
                let mut parts = vec![format!("[{:?}]", node.category), node.role.clone()];
                if let Some(r) = &node.ref_id {
                    parts.push(format!("ref: {}", r));
                }

                if change.old_name != node.name {
                    parts.push(format!("name: {:?} -> {:?}", change.old_name, node.name));
                } else if !node.name.is_empty() {
                    parts.push(format!("name: {:?}", node.name));
                }

                if change.old_value != node.value {
                    parts.push(format!("value: {:?} -> {:?}", change.old_value, node.value));
                } else if !node.value.is_empty() {
                    parts.push(format!("value: {:?}", node.value));
                }

                out.push_str(&format!("  {}\n", parts.join(" | ")));
            }
        }
        out
    }
}

fn format_node(node: &TreeNode) -> String {
    let mut parts = vec![format!("[{:?}]", node.category), node.role.clone()];
    if let Some(r) = &node.ref_id {
        parts.push(format!("ref: {}", r));
    }
    if !node.name.is_empty() {
        parts.push(format!("name: {:?}", node.name));
    }
    if !node.value.is_empty() {
        parts.push(format!("value: {:?}", node.value));
    }
    parts.join(" | ")
}

pub fn compute_diff(old_tree: &TreeNode, new_tree: &TreeNode) -> TreeDiff {
    let mut old_map = HashMap::new();
    let mut new_map = HashMap::new();

    flatten_tree(old_tree, &mut old_map);
    flatten_tree(new_tree, &mut new_map);

    let mut diff = TreeDiff {
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
    };

    for (key, new_node) in &new_map {
        if let Some(old_node) = old_map.get(key) {
            let role_changed = old_node.role != new_node.role;
            let name_changed = old_node.name != new_node.name;
            let value_changed = old_node.value != new_node.value;

            if role_changed || name_changed || value_changed {
                diff.changed.push(ChangedNode {
                    node: (*new_node).clone(),
                    old_role: old_node.role.clone(),
                    old_name: old_node.name.clone(),
                    old_value: old_node.value.clone(),
                });
            }
        } else {
            diff.added.push((*new_node).clone());
        }
    }

    for (key, old_node) in &old_map {
        if !new_map.contains_key(key) {
            diff.removed.push((*old_node).clone());
        }
    }

    diff
}

fn flatten_tree<'a>(node: &'a TreeNode, map: &mut HashMap<String, &'a TreeNode>) {
    let key = if let Some(backend_id) = &node.backend_node_id {
        format!("backend:{:?}", backend_id)
    } else {
        format!("id:{}", node.id)
    };
    map.insert(key, node);

    for child in &node.children {
        flatten_tree(child, map);
    }
}

pub fn serialize_full_tree(node: &TreeNode) -> String {
    serialize_full_tree_capped(node, None)
}

/// Phase 38: variant of [`serialize_full_tree`] that stops emitting
/// nodes once the emitted count reaches `cap`. The cap counts every
/// node line written (root + all descendants); the root itself is
/// always emitted so a truncated tree still carries the page's
/// root-context line. `cap = None` reproduces the pre-Phase-38
/// unbounded behavior byte-for-byte.
///
/// Implementation note: this is a pre-order walk with a shared
/// mutable counter threaded through the recursive helper. The cap is
/// checked *after* the node's own line is written, so a tree of
/// exactly `cap` nodes emits `cap` lines and stops. We never edit
/// the source `TreeNode` — the underlying tree is left intact for
/// the diff path and the resilience hook.
pub fn serialize_full_tree_capped(node: &TreeNode, cap: Option<usize>) -> String {
    let mut out = String::new();
    let mut written = 0usize;
    serialize_full_tree_recursive_capped(node, 0, cap, &mut written, &mut out);
    out
}

fn serialize_full_tree_recursive_capped(
    node: &TreeNode,
    depth: usize,
    cap: Option<usize>,
    written: &mut usize,
    out: &mut String,
) {
    // Always emit the current node — even if we're already at the
    // cap, dropping the line would leave the LLM with a tree that
    // has no root context. Better to write one extra line than to
    // produce a half-formed tree.
    let indent = "  ".repeat(depth);
    out.push_str(&format!("{}{}\n", indent, format_node(node)));
    *written += 1;

    let remaining = match cap {
        Some(c) if *written >= c => return,
        Some(c) => c.saturating_sub(*written),
        None => usize::MAX,
    };
    if remaining == 0 {
        return;
    }

    // For large children lists, slice off the tail once we know we
    // can only emit `remaining` more. This avoids a deep recursion
    // when the cap is hit on a node with thousands of children (the
    // case the Maps cap is designed to protect against).
    if node.children.len() > remaining {
        for child in node.children.iter().take(remaining) {
            serialize_full_tree_recursive_capped(child, depth + 1, Some(remaining), written, out);
            if *written >= cap.unwrap_or(usize::MAX) {
                return;
            }
        }
        return;
    }

    for child in &node.children {
        serialize_full_tree_recursive_capped(child, depth + 1, cap, written, out);
        if *written >= cap.unwrap_or(usize::MAX) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeCategory, TreeNode};

    fn make_node(id: &str, role: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            id: id.to_string(),
            role: role.to_string(),
            name: String::new(),
            value: String::new(),
            category: NodeCategory::Structural,
            ref_id: None,
            backend_node_id: None,
            children,
        }
    }

    fn count_lines(s: &str) -> usize {
        // Each node line is terminated by `\n`. Trim the trailing
        // empty line so we count emitted nodes, not newlines.
        s.lines().count()
    }

    /// Building a tree of N nodes and serializing with no cap must
    /// emit exactly N lines — a regression baseline for the cap
    /// tests below.
    #[test]
    fn uncapped_serialize_matches_node_count() {
        // root + 2 children = 3 nodes
        let tree = make_node(
            "root",
            "RootWebArea",
            vec![
                make_node("c1", "StaticText", vec![]),
                make_node("c2", "StaticText", vec![]),
            ],
        );
        let s = serialize_full_tree(&tree);
        assert_eq!(count_lines(&s), 3);
    }

    /// `cap = None` must produce the same output as the pre-Phase-38
    /// `serialize_full_tree` — this is the byte-for-byte backward
    /// compatibility guarantee.
    #[test]
    fn serialize_full_tree_capped_none_matches_uncapped() {
        let tree = make_node(
            "root",
            "RootWebArea",
            vec![
                make_node("c1", "StaticText", vec![]),
                make_node("c2", "StaticText", vec![]),
            ],
        );
        let a = serialize_full_tree(&tree);
        let b = serialize_full_tree_capped(&tree, None);
        assert_eq!(a, b);
    }

    /// Cap smaller than the tree must truncate. The root is always
    /// emitted so the LLM has at least one line of page context.
    #[test]
    fn cap_truncates_emitted_lines() {
        // 3 nodes total; cap = 2 -> root + 1 child.
        let tree = make_node(
            "root",
            "RootWebArea",
            vec![
                make_node("c1", "StaticText", vec![]),
                make_node("c2", "StaticText", vec![]),
            ],
        );
        let s = serialize_full_tree_capped(&tree, Some(2));
        assert_eq!(count_lines(&s), 2);
        // First emitted line must be the root so the LLM has
        // context even on a truncated tree.
        assert!(s.lines().next().unwrap().contains("RootWebArea"));
    }

    /// Cap exactly equal to the node count emits all of them.
    #[test]
    fn cap_equal_to_node_count_emits_all() {
        let tree = make_node(
            "root",
            "RootWebArea",
            vec![
                make_node("c1", "StaticText", vec![]),
                make_node("c2", "StaticText", vec![]),
            ],
        );
        let s = serialize_full_tree_capped(&tree, Some(3));
        assert_eq!(count_lines(&s), 3);
    }

    /// A cap of 1 must still emit the root (and stop before any
    /// children) so a degenerate config never produces an empty
    /// tree.
    #[test]
    fn cap_of_one_still_emits_root() {
        let tree = make_node(
            "root",
            "RootWebArea",
            vec![make_node("c1", "StaticText", vec![])],
        );
        let s = serialize_full_tree_capped(&tree, Some(1));
        assert_eq!(count_lines(&s), 1);
        assert!(s.contains("RootWebArea"));
    }

    /// Maps-panel shape: a root with one panel and a deep list of
    /// children. Cap at 800 must never panic on a wide tree and
    /// must keep within the cap. We use a wide-but-shallow fixture
    /// (100 direct children) so the test isolates the
    /// `children.len() > remaining` path.
    #[test]
    fn cap_handles_wide_children_list_without_panic() {
        let children: Vec<TreeNode> = (0..100)
            .map(|i| make_node(&format!("c{i}"), "ListItem", vec![]))
            .collect();
        let tree = make_node("root", "RootWebArea", children);
        let s = serialize_full_tree_capped(&tree, Some(20));
        // 1 root + 19 children = 20 lines.
        assert_eq!(count_lines(&s), 20);
    }
}

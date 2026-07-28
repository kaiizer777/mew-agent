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
    let mut out = String::new();
    serialize_full_tree_recursive(node, 0, &mut out);
    out
}

fn serialize_full_tree_recursive(node: &TreeNode, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    out.push_str(&format!("{}{}\n", indent, format_node(node)));
    for child in &node.children {
        serialize_full_tree_recursive(child, depth + 1, out);
    }
}

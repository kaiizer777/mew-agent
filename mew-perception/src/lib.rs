use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::accessibility::{AxNode, AxValue, GetFullAxTreeParams};
use std::collections::HashMap;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeCategory {
    Interactive,
    Content,
    Structural,
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub id: String,
    pub role: String,
    pub name: String,
    pub value: String,
    pub category: NodeCategory,
    pub children: Vec<TreeNode>,
}

const INTERACTIVE_ROLES: &[&str] = &[
    "button", "link", "textbox", "checkbox", "combobox", "radio", 
    "menuitem", "tab", "switch", "slider", "spinbutton", "searchbox", 
    "listbox", "option",
];

const CONTENT_ROLES: &[&str] = &[
    "heading", "text", "cell", "paragraph", "image", "math", "statictext", 
    "linebreak", "graphics-document", "graphics-symbol", "string", "inlinetextbox", "labeltext"
];

fn categorize_role(role: &str) -> NodeCategory {
    let lower_role = role.to_lowercase();
    let is_match = |roles: &[&str]| roles.iter().any(|&r| r == lower_role);
    
    if is_match(INTERACTIVE_ROLES) {
        NodeCategory::Interactive
    } else if is_match(CONTENT_ROLES) {
        NodeCategory::Content
    } else {
        // Anything not explicitly Interactive or Content defaults to Structural
        NodeCategory::Structural
    }
}

fn extract_string(val: &Option<AxValue>) -> String {
    if let Some(v) = val {
        if let Some(s) = &v.value {
            if let Some(str_val) = s.as_str() {
                return str_val.to_string();
            } else {
                return s.to_string();
            }
        }
    }
    String::new()
}

pub fn build_tree(nodes: Vec<AxNode>, compact: bool) -> Option<TreeNode> {
    let mut node_map = HashMap::new();
    let mut child_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut root_id = None;
    
    for node in nodes {
        let id = node.node_id.as_ref().to_string();
        let role = extract_string(&node.role);
        
        // Skip some nodes that don't even have a meaningful role or are purely internal
        // unless they are explicitly roots.
        
        let name = extract_string(&node.name);
        let value = extract_string(&node.value);
        let category = categorize_role(&role);
        
        let t_node = TreeNode {
            id: id.clone(),
            role,
            name,
            value,
            category,
            children: Vec::new(),
        };
        
        node_map.insert(id.clone(), t_node);
        
        if let Some(parent) = &node.parent_id {
            child_map.entry(parent.as_ref().to_string()).or_default().push(id.clone());
        } else {
            root_id = Some(id.clone());
        }
    }
    
    if root_id.is_none() {
        for (id, node) in &node_map {
            if node.role == "RootWebArea" {
                root_id = Some(id.clone());
                break;
            }
        }
    }
    
    // In case no parent_id was None, and no RootWebArea was found, just pick the first node (rare fallback)
    if root_id.is_none() {
        if let Some(id) = node_map.keys().next() {
            root_id = Some(id.clone());
        }
    }
    
    let root = root_id?;
    
    fn assemble(id: &str, node_map: &HashMap<String, TreeNode>, child_map: &HashMap<String, Vec<String>>) -> Option<TreeNode> {
        let mut node = node_map.get(id)?.clone();
        
        if let Some(children_ids) = child_map.get(id) {
            for child_id in children_ids {
                if let Some(child_node) = assemble(child_id, node_map, child_map) {
                    node.children.push(child_node);
                }
            }
        }
        
        Some(node)
    }
    
    let mut root_node = assemble(&root, &node_map, &child_map)?;
    
    if compact {
        fn prune(mut node: TreeNode) -> Option<TreeNode> {
            let children = std::mem::replace(&mut node.children, Vec::new());
            let mut pruned_children = Vec::new();
            
            let is_static_text_with_name = node.role.eq_ignore_ascii_case("statictext") && !node.name.is_empty();
            
            for child in children {
                if is_static_text_with_name && child.role.eq_ignore_ascii_case("inlinetextbox") {
                    continue;
                }
                
                if let Some(pruned_child) = prune(child) {
                    pruned_children.push(pruned_child);
                }
            }
            
            node.children = pruned_children;
            
            if node.category == NodeCategory::Structural && node.name.is_empty() && node.value.is_empty() {
                if node.children.is_empty() {
                    return None;
                } else if node.children.len() == 1 {
                    return Some(node.children.into_iter().next().unwrap());
                }
            }
            
            Some(node)
        }
        
        root_node = prune(root_node)?;
    }
    
    Some(root_node)
}

impl TreeNode {
    pub fn print(&self, depth: usize) {
        let indent = "  ".repeat(depth);
        let mut parts = vec![format!("[{:?}]", self.category), self.role.clone()];
        if !self.name.is_empty() {
            parts.push(format!("name: {:?}", self.name));
        }
        if !self.value.is_empty() {
            parts.push(format!("value: {:?}", self.value));
        }
        println!("{}{}", indent, parts.join(" | "));
        
        for child in &self.children {
            child.print(depth + 1);
        }
    }
}

pub async fn extract_tree(page: &Page, compact: bool) -> anyhow::Result<(TreeNode, std::time::Duration)> {
    let start = Instant::now();
    let res = page.execute(GetFullAxTreeParams::default()).await?;
    let duration = start.elapsed();
    
    let root = build_tree(res.nodes.clone(), compact).ok_or_else(|| anyhow::anyhow!("Failed to build tree: no root found"))?;
    Ok((root, duration))
}

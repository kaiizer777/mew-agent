use crate::TreeNode;
use std::collections::HashMap;

#[derive(Default, Debug)]
pub struct PerceptionState {
    pub history: HashMap<String, TreeNode>,
}

impl PerceptionState {
    pub fn new() -> Self {
        Self {
            history: HashMap::new(),
        }
    }
    
    pub fn get_previous_tree(&self, session_id: &str) -> Option<&TreeNode> {
        self.history.get(session_id)
    }
    
    pub fn save_tree(&mut self, session_id: &str, tree: TreeNode) {
        self.history.insert(session_id.to_string(), tree);
    }
    
    pub fn clear(&mut self, session_id: &str) {
        self.history.remove(session_id);
    }
}

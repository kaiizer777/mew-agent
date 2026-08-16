use crate::TreeNode;
use std::collections::HashMap;

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SessionSnapshotStats {
    pub full_snapshots: usize,
    pub diff_snapshots: usize,
}

impl SessionSnapshotStats {
    pub fn total(&self) -> usize {
        self.full_snapshots + self.diff_snapshots
    }

    pub fn full_ratio(&self) -> f64 {
        if self.total() == 0 {
            0.0
        } else {
            self.full_snapshots as f64 / self.total() as f64
        }
    }

    pub fn diff_ratio(&self) -> f64 {
        if self.total() == 0 {
            0.0
        } else {
            self.diff_snapshots as f64 / self.total() as f64
        }
    }
}

#[derive(Default, Debug)]
pub struct PerceptionState {
    pub history: HashMap<String, TreeNode>,
    pub stats: HashMap<String, SessionSnapshotStats>,
}

impl PerceptionState {
    pub fn new() -> Self {
        Self {
            history: HashMap::new(),
            stats: HashMap::new(),
        }
    }

    pub fn get_previous_tree(&self, session_id: &str) -> Option<&TreeNode> {
        self.history.get(session_id)
    }

    pub fn save_tree(&mut self, session_id: &str, tree: TreeNode) {
        self.history.insert(session_id.to_string(), tree);
    }

    pub fn record_full_snapshot(&mut self, session_id: &str) {
        let entry = self.stats.entry(session_id.to_string()).or_default();
        entry.full_snapshots += 1;
    }

    pub fn record_diff_snapshot(&mut self, session_id: &str) {
        let entry = self.stats.entry(session_id.to_string()).or_default();
        entry.diff_snapshots += 1;
    }

    pub fn get_stats(&self, session_id: &str) -> SessionSnapshotStats {
        self.stats.get(session_id).cloned().unwrap_or_default()
    }

    pub fn clear(&mut self, session_id: &str) {
        self.history.remove(session_id);
        self.stats.remove(session_id);
    }
}


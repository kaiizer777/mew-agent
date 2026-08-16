use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Perception pruning rules for compact snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerceptionPruningRule {
    /// Maximum character length for non-interactive text values before truncation (0 = unlimited).
    #[serde(default = "default_max_text_len")]
    pub max_content_text_length: usize,
    /// Roles to aggressively prune when they contain no interactive descendants.
    #[serde(default = "default_prune_roles")]
    pub prune_roles: Vec<String>,
    /// Whether to prune empty structural containers.
    #[serde(default = "default_true")]
    pub prune_empty_containers: bool,
    /// Target maximum size threshold in KB.
    #[serde(default = "default_target_kb")]
    pub target_max_kb: usize,
}

fn default_max_text_len() -> usize {
    300
}

fn default_prune_roles() -> Vec<String> {
    vec![
        "generic".to_string(),
        "banner".to_string(),
        "complementary".to_string(),
        "contentinfo".to_string(),
        "presentation".to_string(),
        "none".to_string(),
    ]
}

fn default_true() -> bool {
    true
}

fn default_target_kb() -> usize {
    5
}

impl Default for PerceptionPruningRule {
    fn default() -> Self {
        Self {
            max_content_text_length: default_max_text_len(),
            prune_roles: default_prune_roles(),
            prune_empty_containers: true,
            target_max_kb: default_target_kb(),
        }
    }
}

/// Overrides table mapping domains to site-specific pruning rules.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PerceptionOverrides {
    #[serde(default)]
    pub default: PerceptionPruningRule,
    #[serde(default)]
    pub overrides: HashMap<String, PerceptionPruningRule>,
}

impl PerceptionOverrides {
    pub fn load_from_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn load_from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::load_from_str(&content)?)
    }

    pub fn find_config_file() -> Option<PathBuf> {
        let candidates = [
            PathBuf::from("config/perception_overrides.toml"),
            PathBuf::from("../config/perception_overrides.toml"),
            PathBuf::from("../../config/perception_overrides.toml"),
        ];
        for path in &candidates {
            if path.exists() {
                return Some(path.clone());
            }
        }
        None
    }

    pub fn load_default_or_fallback() -> Self {
        if let Some(p) = Self::find_config_file() {
            if let Ok(cfg) = Self::load_from_file(&p) {
                return cfg;
            }
        }
        Self::default()
    }

    pub fn rule_for_domain(&self, domain: Option<&str>) -> PerceptionPruningRule {
        if let Some(d) = domain {
            let normalized = d.trim().to_lowercase();
            let host = normalized.trim_start_matches("www.");
            if let Some(r) = self.overrides.get(host) {
                return r.clone();
            }
            for (key, r) in &self.overrides {
                if host == key || host.ends_with(&format!(".{}", key)) {
                    return r.clone();
                }
            }
        }
        self.default.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_overrides_toml() {
        let toml_str = r#"
[default]
max_content_text_length = 300
prune_roles = ["generic", "banner"]
prune_empty_containers = true
target_max_kb = 5

[overrides."linkedin.com"]
max_content_text_length = 150
prune_roles = ["generic", "presentation"]
prune_empty_containers = true
"#;
        let overrides = PerceptionOverrides::load_from_str(toml_str).unwrap();
        assert_eq!(overrides.default.max_content_text_length, 300);
        let rule = overrides.rule_for_domain(Some("www.linkedin.com"));
        assert_eq!(rule.max_content_text_length, 150);
        let fallback = overrides.rule_for_domain(Some("unknown.org"));
        assert_eq!(fallback.max_content_text_length, 300);
    }
}

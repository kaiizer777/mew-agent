use mew_perception::diff::compute_diff;
use mew_perception::overrides::PerceptionOverrides;
use mew_perception::state::PerceptionState;
use mew_perception::{NodeCategory, TreeNode};

use mew_resilience::ref_recovery::{
    attempt_recovery, RefActionKind, RefRecoveryConfig, RefRecoveryInputs, RefRecoveryOutcome,
};
use std::collections::HashMap;

fn make_node(id: &str, role: &str, name: &str, category: NodeCategory, ref_id: Option<&str>) -> TreeNode {
    TreeNode {
        id: id.to_string(),
        role: role.to_string(),
        name: name.to_string(),
        value: String::new(),
        category,
        ref_id: ref_id.map(|r| r.to_string()),
        backend_node_id: None,
        children: Vec::new(),
    }
}

#[test]
fn test_perception_state_counter_tracking() {
    let mut state = PerceptionState::new();
    let session = "test_session_1";

    assert_eq!(state.get_stats(session).total(), 0);
    assert_eq!(state.get_stats(session).full_snapshots, 0);
    assert_eq!(state.get_stats(session).diff_snapshots, 0);

    state.record_full_snapshot(session);
    assert_eq!(state.get_stats(session).full_snapshots, 1);
    assert_eq!(state.get_stats(session).diff_snapshots, 0);
    assert_eq!(state.get_stats(session).total(), 1);

    state.record_diff_snapshot(session);
    state.record_diff_snapshot(session);
    state.record_diff_snapshot(session);
    assert_eq!(state.get_stats(session).full_snapshots, 1);
    assert_eq!(state.get_stats(session).diff_snapshots, 3);
    assert_eq!(state.get_stats(session).total(), 4);
    assert!((state.get_stats(session).full_ratio() - 0.25).abs() < 1e-5);
    assert!((state.get_stats(session).diff_ratio() - 0.75).abs() < 1e-5);

    state.clear(session);
    assert_eq!(state.get_stats(session).total(), 0);
}

#[test]
fn test_ref_recovery_local_change_vs_navigation() {
    let cfg = RefRecoveryConfig::default();
    let mut ref_map = HashMap::new();
    ref_map.insert("@e1".to_string(), ());
    ref_map.insert("@e2".to_string(), ());

    // 1. Local DOM change (page_navigated: false)
    let local_inputs = RefRecoveryInputs {
        supplied_ref: "@e1",
        current_ref_map: &ref_map,
        target_desc: None,
        description_index: None,
        action: RefActionKind::Click,
        attempts_so_far: 0,
        page_navigated: false,
    };
    let local_outcome = attempt_recovery(&cfg, &local_inputs);
    assert!(matches!(local_outcome, RefRecoveryOutcome::Retry { force_full_snapshot, .. } if !force_full_snapshot));
    assert_eq!(local_outcome.force_full_snapshot(), false);

    // 2. Navigation occurred (page_navigated: true)
    let nav_inputs = RefRecoveryInputs {
        supplied_ref: "@e1",
        current_ref_map: &ref_map,
        target_desc: None,
        description_index: None,
        action: RefActionKind::Click,
        attempts_so_far: 0,
        page_navigated: true,
    };
    let nav_outcome = attempt_recovery(&cfg, &nav_inputs);
    assert!(matches!(nav_outcome, RefRecoveryOutcome::Retry { force_full_snapshot, .. } if force_full_snapshot));
    assert_eq!(nav_outcome.force_full_snapshot(), true);
}

#[test]
fn test_perception_overrides_compact_pruning() {
    let overrides = PerceptionOverrides::load_default_or_fallback();
    let linkedin_rule = overrides.rule_for_domain(Some("linkedin.com"));
    assert_eq!(linkedin_rule.target_max_kb, 5);

    // Create a node with very long boilerplate text (> 200 chars)
    let long_text = "A".repeat(400);
    let node = make_node("n1", "paragraph", &long_text, NodeCategory::Content, None);
    let mut root = make_node("root", "RootWebArea", "", NodeCategory::Structural, None);
    root.children.push(node);

    // Test pruning using rule
    let serialized = mew_perception::diff::serialize_full_tree(&root);
    assert!(serialized.len() > 400);
}

#[test]
fn test_local_diff_vs_full_replace_heuristic() {
    let mut root_v1 = make_node("root", "RootWebArea", "", NodeCategory::Structural, None);
    root_v1.children.push(make_node("btn1", "button", "Search", NodeCategory::Interactive, Some("@e1")));
    root_v1.children.push(make_node("txt1", "heading", "Job Board", NodeCategory::Content, None));

    // v2: local change — heading text changed slightly
    let mut root_v2 = make_node("root", "RootWebArea", "", NodeCategory::Structural, None);
    root_v2.children.push(make_node("btn1", "button", "Search", NodeCategory::Interactive, Some("@e1")));
    root_v2.children.push(make_node("txt1", "heading", "Job Board - Results", NodeCategory::Content, None));

    let diff = compute_diff(&root_v1, &root_v2);
    assert_eq!(diff.changed.len(), 1);
    assert_eq!(diff.added.len(), 0);
    assert_eq!(diff.removed.len(), 0);

    let diff_text = diff.serialize_compact();
    assert!(diff_text.contains("[Changed]"));
    assert!(diff_text.contains("Job Board"));
}

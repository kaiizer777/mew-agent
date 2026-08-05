// mew v2 — Phase 11: Todo schema and decomposition contract tests.

use mew_agent::handoff::Handoff;
use mew_agent::planner::{decompose_to_todos, slugify};
use mew_agent::todo::{
    AcceptanceCriterion, AcceptanceKind, Evidence, Todo, TodoBudget, TodoId, TodoStatus,
};

#[test]
fn test_todo_id_from_slug_matches_planner_slugify() {
    let desc = "go to instagram";
    let index = 0;
    let todo_id = TodoId::from_slug(desc, index);
    let expected_slug = slugify(desc, index);

    assert_eq!(todo_id.as_str(), expected_slug);
    assert_eq!(todo_id.to_string(), expected_slug);
    assert_eq!(&*todo_id, expected_slug.as_str());
    assert_eq!(todo_id.as_ref(), expected_slug.as_str());
}

#[test]
fn test_slug_idempotency_prefix() {
    let input1 = "a b c";
    let input2 = "a-b-c";
    let index = 0;

    let slug1 = slugify(input1, index);
    let slug2 = slugify(input2, index);

    assert_eq!(slug1, slug2);
    assert_eq!(slug1, "a-b-c-1");
}

#[test]
fn test_todo_json_roundtrip() {
    let todo = Todo {
        id: TodoId::from_slug("navigate instagram", 0),
        intent: "navigate instagram".to_string(),
        acceptance: Some(AcceptanceCriterion::new(
            AcceptanceKind::UrlAt,
            "https://instagram.com",
        )),
        depends_on: vec![],
        status: TodoStatus::Pending,
        evidence: None,
        attempts: 0,
        budget: TodoBudget::default(),
        last_evidence_iteration: None,
    };

    let json = serde_json::to_string(&todo).expect("serialize todo");
    let deserialized: Todo = serde_json::from_str(&json).expect("deserialize todo");

    assert_eq!(todo, deserialized);

    // Verify #[serde(tag = "type")] on AcceptanceKind
    let kind = AcceptanceKind::UrlAt;
    let kind_json = serde_json::to_string(&kind).expect("serialize AcceptanceKind");
    assert!(kind_json.contains(r#""type":"UrlAt""#));
}

#[test]
fn test_evidence_json_roundtrip() {
    let evidence = Evidence {
        todo_id: TodoId("step-1".to_string()),
        worker_signature: "len:00000100".to_string(),
        planner_signature: "len:00000100".to_string(),
        verified_at_secs: 1700000000,
    };

    let json = serde_json::to_string(&evidence).expect("serialize evidence");
    let deserialized: Evidence = serde_json::from_str(&json).expect("deserialize evidence");

    assert_eq!(evidence, deserialized);
}

#[test]
fn test_decompose_to_todos_instagram_regression() {
    let handoff = Handoff::bare("go to instagram and text my friend hi", "msg-101");
    let todos = decompose_to_todos(&handoff);

    assert_eq!(todos.len(), 2);

    assert_eq!(todos[0].id.as_str(), "go-to-instagram-1");
    assert_eq!(todos[0].intent, "go to instagram");
    assert_eq!(todos[0].status, TodoStatus::Pending);
    assert_eq!(todos[0].depends_on, Vec::<TodoId>::new());
    assert_eq!(
        todos[0].acceptance,
        Some(AcceptanceCriterion::new(
            AcceptanceKind::UrlAt,
            "go to instagram"
        ))
    );

    assert_eq!(todos[1].id.as_str(), "text-my-friend-hi-2");
    assert_eq!(todos[1].intent, "text my friend hi");
    assert_eq!(todos[1].status, TodoStatus::Pending);
    assert_eq!(todos[1].depends_on, vec![TodoId("go-to-instagram-1".to_string())]);
    assert_eq!(
        todos[1].acceptance,
        Some(AcceptanceCriterion::new(
            AcceptanceKind::ElementPresent,
            "text my friend hi"
        ))
    );
}

#[test]
fn test_decompose_to_todos_fallback_acceptance() {
    let handoff = Handoff::bare("search for low cost flights", "msg-102");
    let todos = decompose_to_todos(&handoff);

    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].id.as_str(), "search-for-low-cost-flights-1");
    assert_eq!(
        todos[0].acceptance,
        Some(AcceptanceCriterion::new(
            AcceptanceKind::AnySnapshot,
            "search for low cost flights"
        ))
    );
}

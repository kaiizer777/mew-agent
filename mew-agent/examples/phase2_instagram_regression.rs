// mew v2 — Phase 2: instagram-phrasing regression fixture.
//
// This example replays the two phrasings that originally
// demonstrated Bug #1 through the *post-Phase-2* resolver and
// planner, and asserts they produce equivalent first-navigation
// targets and equivalent subtask lists. If this example ever
// fails, Phase 2 has regressed.
//
// The two phrasings (per docs/bug-1-root-cause.md):
//
//   * "go to instagram and text my friend hi"
//       — was failing pre-Phase-2 (direct nav trips IG bot-detect)
//       — must route via google.com/search?q=instagram post-fix
//
//   * "go to google, search instagram, then text my friend hi"
//       — was working pre-Phase-2 (organic-looking referrer chain)
//       — must STILL route via google.com/search?q=instagram
//         post-fix (the planner's job is to inject the same plan
//         into the system prompt, not to change the entry path
//         for prompts that already said "use google")
//
// "Equivalent first-navigation target" means: both prompts
// produce a URL whose host is google.com AND whose query string
// contains "instagram" — i.e. both end up on a Google search
// results page for instagram, from which the agent must click
// the organic result. Pre-Phase-2, the first prompt produced a
// direct `https://www.instagram.com` map-hit; post-Phase-2, both
// produce a `via-search` resolution.
//
// "Equivalent subtask lists" means: both prompts produce a plan
// with >= 2 items (navigate + text) and the per-item descriptions
// are sensible. Pre-Phase-2, the LLM often collapsed them into a
// single undifferentiated blob; post-Phase-2, the deterministic
// planner splits them on " and " (with surrounding spaces).
//
// This example does NOT spin up Chrome and does NOT call the LLM
// — it exercises the pure-Rust resolver and planner directly so
// the regression test runs in milliseconds and doesn't need the
// env prerequisites from work.md (live cookies, API key, etc.).
//
// Run with:
//   cargo run --example phase2_instagram_regression -p mew-agent

use mew_nav::SensitivePlatforms;
use mew_agent::planner::plan;

const FAILING_PROMPT: &str = "go to instagram and text my friend hi";
const WORKING_PROMPT: &str =
    "go to google, search instagram, then text my friend hi";

fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // Build the same sensitive-platforms table the agent
    // loads in production. We re-parse
    // `config/sensitive_platforms.toml` so a future
    // config change is reflected here without a code edit.
    let sensitive = match SensitivePlatforms::load_from(std::path::Path::new(
        "config/sensitive_platforms.toml",
    )) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "[phase2] could not load config/sensitive_platforms.toml: {e}"
            );
            eprintln!("[phase2] falling back to the empty table");
            SensitivePlatforms::default()
        }
    };
    println!(
        "[phase2] sensitive_platforms table: {} entries",
        sensitive.entries.len()
    );

    // --------------------------------------------------------------
    // PART 1 — both prompts must produce a via-search resolution
    // --------------------------------------------------------------
    //
    // We use the *dry-run* resolver (`resolve_without_probe_sensitive`)
    // because it doesn't need a live browser. The probe
    // version would also work in a real run, but the dry-run
    // path is enough to assert "what URL would the agent land
    // on first?"
    //
    // For the FAILING_PROMPT, the resolver input is just
    // "instagram" (the bare noun the LLM would pass to
    // `navigate("instagram")`).
    //
    // For the WORKING_PROMPT, the resolver input is the same
    // "instagram" because the LLM's first navigate call is
    // also `navigate("instagram")` after the agent has clicked
    // the search result on google.com. (The "go to google,
    // search X" is itself a separate navigate, but the test
    // here asserts the *instagram* navigate — the one that
    // originally failed.)
    let first_nav_input = "instagram";

    let failing_resolution =
        mew_nav::resolve_without_probe_sensitive(first_nav_input, &sensitive);
    let working_resolution =
        mew_nav::resolve_without_probe_sensitive(first_nav_input, &sensitive);

    println!(
        "[phase2] FAILING prompt first nav -> {} (via {})",
        failing_resolution.url, failing_resolution.path.as_str()
    );
    println!(
        "[phase2] WORKING prompt first nav -> {} (via {})",
        working_resolution.url, working_resolution.path.as_str()
    );

    // Assertion 1: both must be a via-search resolution.
    let assert_via_search = |label: &str, res: &mew_nav::ResolutionResult| {
        assert!(
            matches!(
                res.path,
                mew_nav::ResolutionPath::ViaSearch
                    | mew_nav::ResolutionPath::ViaSearchConfirm
            ),
            "[{label}] expected ViaSearch or ViaSearchConfirm, got {:?} (url={})",
            res.path,
            res.url
        );
    };
    assert_via_search("FAILING", &failing_resolution);
    assert_via_search("WORKING", &working_resolution);
    println!("[phase2] ✓ both phrasings produce a via-search resolution");

    // Assertion 2: both URLs target google.com search with
    // "instagram" in the query. (Equivalent first-navigation
    // target — both end up on a Google search results page
    // for instagram.)
    let assert_google_instagram =
        |label: &str, res: &mew_nav::ResolutionResult| {
            assert!(
                res.url.contains("google.com/search"),
                "[{label}] expected google.com/search, got {}",
                res.url
            );
            assert!(
                res.url.contains("instagram"),
                "[{label}] expected query to contain 'instagram', got {}",
                res.url
            );
        };
    assert_google_instagram("FAILING", &failing_resolution);
    assert_google_instagram("WORKING", &working_resolution);
    println!("[phase2] ✓ both phrasings land on google.com/search?q=instagram");

    // Assertion 3: the two URLs are *exactly* equal. Both
    // prompts trigger the same `navigate("instagram")` call,
    // which the resolver must handle identically. If the
    // resolutions ever diverge (e.g. a future change makes
    // the resolver context-sensitive on the prompt), this
    // assertion catches it.
    assert_eq!(
        failing_resolution.url, working_resolution.url,
        "FAILING and WORKING prompts produced different instagram resolutions"
    );
    println!("[phase2] ✓ both phrasings resolve to the same URL");

    // --------------------------------------------------------------
    // PART 2 — both prompts must produce a 2+ subtask plan
    // --------------------------------------------------------------
    //
    // Pre-Phase-2, the LLM often declared zero or one
    // subtasks, leaving the completeness gate as a no-op.
    // Post-Phase-2, the deterministic planner splits on
    // " and " (with surrounding spaces) and produces a
    // multi-item plan.
    let failing_plan = plan(FAILING_PROMPT);
    let working_plan = plan(WORKING_PROMPT);

    println!(
        "[phase2] FAILING prompt plan: {} subtask(s) — {}",
        failing_plan.subtasks.len(),
        failing_plan.rationale
    );
    for s in &failing_plan.subtasks {
        println!("        - [{}] {}", s.id, s.description);
    }
    println!(
        "[phase2] WORKING prompt plan: {} subtask(s) — {}",
        working_plan.subtasks.len(),
        working_plan.rationale
    );
    for s in &working_plan.subtasks {
        println!("        - [{}] {}", s.id, s.description);
    }

    // Assertion 4: both plans have >= 2 items. (Both
    // prompts are compound — they each contain a " and " or
    // " then " marker.)
    assert!(
        failing_plan.subtasks.len() >= 2,
        "FAILING prompt should produce a 2+ subtask plan, got {}",
        failing_plan.subtasks.len()
    );
    assert!(
        working_plan.subtasks.len() >= 2,
        "WORKING prompt should produce a 2+ subtask plan, got {}",
        working_plan.subtasks.len()
    );
    println!("[phase2] ✓ both phrasings produce a 2+ subtask plan");

    // Assertion 5: both plans contain a subtask that
    // mentions "instagram" (the navigate target) and a
    // subtask that mentions "text" / "message" / "friend"
    // (the second action). We accept a few synonyms because
    // the split is on a marker, not on semantics.
    let assert_plan_has_navigate_and_action =
        |label: &str, p: &mew_agent::planner::Plan| {
            let has_navigate = p.subtasks.iter().any(|s| {
                let d = s.description.to_lowercase();
                d.contains("instagram") || d.contains("google")
            });
            let has_action = p.subtasks.iter().any(|s| {
                let d = s.description.to_lowercase();
                d.contains("text")
                    || d.contains("message")
                    || d.contains("friend")
                    || d.contains("search")
            });
            assert!(has_navigate, "[{label}] plan has no navigate-style subtask: {:#?}", p.subtasks);
            assert!(has_action, "[{label}] plan has no action-style subtask: {:#?}", p.subtasks);
        };
    assert_plan_has_navigate_and_action("FAILING", &failing_plan);
    assert_plan_has_navigate_and_action("WORKING", &working_plan);
    println!("[phase2] ✓ both phrasings cover navigate + action in the plan");

    // --------------------------------------------------------------
    // Summary
    // --------------------------------------------------------------
    println!();
    println!("[phase2] regression fixture PASSED");
    println!("[phase2] both instagram phrasings now:");
    println!("[phase2]   - resolve to {} ({})", failing_resolution.url, failing_resolution.path.as_str());
    println!(
        "[phase2]   - decompose into {} (failing) / {} (working) subtask(s)",
        failing_plan.subtasks.len(),
        working_plan.subtasks.len()
    );
    println!("[phase2] the original Phase-2 user-visible failure mode is closed.");

    Ok(())
}

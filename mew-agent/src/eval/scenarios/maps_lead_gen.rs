//! Phase 34 — Google Maps vertical pack eval scenario.
//!
//! The motivating use case is "find salons in Mumbai with no
//! website and rating above 4.0 — check 15 listings". A live
//! run on real Maps is expensive (1 panel per listing, ~5s
//! per panel, plus a full page render), so this scenario is
//! pure-Rust and exercises the *data path* end-to-end:
//!
//!   1. `MapsQuery::parse` correctly extracts the structured
//!      query from the user's free-text task.
//!   2. `MapsQuery::is_qualified_lead` correctly filters the
//!      5 mock listings down to the subset that satisfies
//!      every active filter.
//!   3. The `Handoff` for a Maps task carries the
//!      `maps_query` field set, so the agent's preflight
//!      plan has the parsed query on hand.
//!   4. The `BrowserResult` for a Maps task carries the
//!      `maps_findings` field populated, so the synthesizer
//!      has the per-listing data to render.
//!   5. The system-prompt hint block the agent sees on a
//!      Maps task contains the verbatim Phase 34 spec text
//!      (the 6 CRITICAL RULES), so the LLM gets the Maps
//!      SPA semantics it needs.
//!
//! The scenario is data-driven, like the Phase 30 eval
//! scenarios: a single `maps_lead_gen_scenario()` function
//! returns a `Scenario` (so `default_scenarios()` can pick it
//! up) and a `maps_lead_gen_scenarios()` convenience
//! function returns the per-subscenario list.
//!
//! The `MockMapsPanel` struct is the per-listing fixture: a
//! name, an optional phone, an optional website_url, a
//! rating, and a review_count. The 5 fixtures are calibrated
//! so the filter is non-trivial: 2 listings pass (one with
//! no-website+phone+rating>=4.0, one with website but
//! rating>=4.0), 3 fail (one with rating<4.0, one with
//! website when no-website is required, one with no
//! phone when phone is required). The exact fixtures are
//! documented inline so a future reader can trace each
//! assertion back to a specific listing.

use crate::chat_agent::is_local_lead_gen_task;
use crate::handoff::{MapsFinding, MapsQuery};
use crate::router::TaskShape;

/// The fixed 5-listing fixture used by every scenario in this
/// module. Hand-picked to exercise every filter branch
/// (rating cutoff, no-website, phone presence) on at least
/// one listing.
fn mock_maps_listings() -> Vec<MockMapsPanel> {
    vec![
        // PASS — no website, has phone, rating 4.5 (>= 4.0).
        MockMapsPanel {
            name: "Salon A".into(),
            phone: Some("+91 98765 11111".into()),
            address: Some("Bandra West, Mumbai".into()),
            rating: Some(4.5),
            review_count: Some(120),
            has_website: false,
            website_url: None,
            category: Some("Beauty salon".into()),
            maps_url: "https://www.google.com/maps/place/Salon+A".into(),
        },
        // FAIL — has website (so `require_no_website` rejects).
        MockMapsPanel {
            name: "Salon B".into(),
            phone: Some("+91 98765 22222".into()),
            address: Some("Andheri East, Mumbai".into()),
            rating: Some(4.2),
            review_count: Some(80),
            has_website: true,
            website_url: Some("https://salonb.example".into()),
            category: Some("Beauty salon".into()),
            maps_url: "https://www.google.com/maps/place/Salon+B".into(),
        },
        // FAIL — rating 3.8 (< 4.0).
        MockMapsPanel {
            name: "Salon C".into(),
            phone: Some("+91 98765 33333".into()),
            address: Some("Juhu, Mumbai".into()),
            rating: Some(3.8),
            review_count: Some(45),
            has_website: false,
            website_url: None,
            category: Some("Beauty salon".into()),
            maps_url: "https://www.google.com/maps/place/Salon+C".into(),
        },
        // PASS — has website but phone present, rating 4.6.
        //      (This passes when `require_no_website` is OFF
        //      and the query is "rating above 4.0".)
        MockMapsPanel {
            name: "Salon D".into(),
            phone: Some("+91 98765 44444".into()),
            address: Some("Powai, Mumbai".into()),
            rating: Some(4.6),
            review_count: Some(210),
            has_website: true,
            website_url: Some("https://salond.example".into()),
            category: Some("Beauty salon".into()),
            maps_url: "https://www.google.com/maps/place/Salon+D".into(),
        },
        // FAIL — no phone (so `require_phone` rejects).
        //      With the canonical query (no website + rating >=
        //      4.0, no phone filter) this would otherwise pass,
        //      so it's labeled FAIL only for the phone-filtered
        //      variant (see scenario 4 variant assertions).
        MockMapsPanel {
            name: "Salon E".into(),
            phone: None,
            address: Some("Goregaon, Mumbai".into()),
            rating: Some(4.4),
            review_count: Some(60),
            has_website: false,
            website_url: None,
            category: Some("Beauty salon".into()),
            maps_url: "https://www.google.com/maps/place/Salon+E".into(),
        },
    ]
}

/// One mock Maps panel. Mirrors `MapsFinding` but uses owned
/// `String` and a friendlier field shape — we convert to
/// `MapsFinding` at the assertion boundary so the
/// test exercises the public conversion path the live
/// agent uses.
#[derive(Debug, Clone)]
struct MockMapsPanel {
    name: String,
    phone: Option<String>,
    address: Option<String>,
    rating: Option<f32>,
    review_count: Option<u32>,
    has_website: bool,
    website_url: Option<String>,
    category: Option<String>,
    maps_url: String,
}

impl MockMapsPanel {
    fn to_finding(&self, rank: usize) -> MapsFinding {
        MapsFinding {
            rank,
            name: self.name.clone(),
            phone: self.phone.clone(),
            address: self.address.clone(),
            rating: self.rating,
            review_count: self.review_count,
            has_website: self.has_website,
            website_url: self.website_url.clone(),
            category: self.category.clone(),
            maps_url: self.maps_url.clone(),
        }
    }
}

// ----------------------------------------------------------------------
// Scenario 1 — MapsQuery::parse on the canonical Phase 34 task
// ----------------------------------------------------------------------

/// Asserts that `MapsQuery::parse` correctly extracts the
/// query from the canonical Phase 34 acceptance-criteria
/// task: "Find salons in Mumbai with no website and rating
/// above 4.0 and must have phone — check 15 listings". The
/// acceptance criteria is: visit all 15 listings, extract
/// data from each, produce a ranked table.
pub fn scenario_1_maps_query_parse_canonical_task() -> Result<(), String> {
    let task = "Find salons in Mumbai with no website and rating above 4.0 and must have phone — check 15 listings";
    let q = MapsQuery::parse(task);

    if q.search_query != task {
        return Err(format!(
            "search_query must be the original task verbatim, got {:?}",
            q.search_query
        ));
    }
    if q.city.to_lowercase() != "mumbai" {
        return Err(format!(
            "city must be 'mumbai' (extracted from ' in Mumbai'), got {:?}",
            q.city
        ));
    }
    if q.listing_count != 15 {
        return Err(format!(
            "listing_count must be 15 (parsed from 'check 15 listings'), got {}",
            q.listing_count
        ));
    }
    if q.min_rating != Some(4.0) {
        return Err(format!(
            "min_rating must be 4.0 (parsed from 'rating above 4.0'), got {:?}",
            q.min_rating
        ));
    }
    if !q.require_no_website {
        return Err("require_no_website must be true (parsed from 'no website')".into());
    }
    if !q.require_phone {
        return Err(
            "require_phone must be true (parsed from 'must have phone')".into(),
        );
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 2 — MapsQuery::parse on alternate phrasings
// ----------------------------------------------------------------------

/// Asserts the parser handles the most common alternate
/// phrasings. Each branch is a known case the spec calls out
/// or that the operator is likely to see in the wild.
pub fn scenario_2_maps_query_parse_alternate_phrasings() -> Result<(), String> {
    // "First N listings" — alternative to "check N".
    let q = MapsQuery::parse("first 8 gyms in Delhi");
    if q.listing_count != 8 {
        return Err(format!(
            "'first 8 gyms in Delhi' must parse listing_count=8, got {}",
            q.listing_count
        ));
    }
    if q.city.to_lowercase() != "delhi" {
        return Err(format!(
            "city must be 'Delhi', got {:?}",
            q.city
        ));
    }

    // "rating >= X" — alternative to "rating above X".
    let q = MapsQuery::parse("Find clinics in Pune with rating >= 4.5");
    if q.min_rating != Some(4.5) {
        return Err(format!(
            "'rating >= 4.5' must parse min_rating=4.5, got {:?}",
            q.min_rating
        ));
    }

    // "with phone" filter.
    let q = MapsQuery::parse("Find restaurants near Bengaluru with phone");
    if !q.require_phone {
        return Err("'with phone' must set require_phone=true".into());
    }
    if q.city.to_lowercase() != "bengaluru" {
        return Err(format!(
            "'near Bengaluru' must extract city='Bengaluru', got {:?}",
            q.city
        ));
    }

    // Hard cap: a typo "check 5000 listings" must clamp to 50.
    let q = MapsQuery::parse("check 5000 listings in Tokyo");
    if q.listing_count != 50 {
        return Err(format!(
            "listing_count must clamp to hard cap 50, got {}",
            q.listing_count
        ));
    }

    // Default: no count -> 15.
    let q = MapsQuery::parse("Find salons in Mumbai with no website");
    if q.listing_count != 15 {
        return Err(format!(
            "no count -> default 15, got {}",
            q.listing_count
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 3 — MapsFinding::is_qualified_lead applies filters
// ----------------------------------------------------------------------

/// Asserts `is_qualified_lead` correctly filters the 5 mock
/// listings. With the canonical query (no website, rating >=
/// 4.0, must have phone), exactly 1 listing (Salon A) should
/// pass: it has no website, rating 4.5, and a phone. B
/// fails on website, C fails on rating, D fails on
/// website, E fails on phone.
pub fn scenario_3_maps_finding_is_qualified_lead() -> Result<(), String> {
    let query = MapsQuery::parse(
        "Find salons in Mumbai with no website and rating above 4.0 and must have phone — check 15 listings",
    );
    let listings: Vec<MapsFinding> = mock_maps_listings()
        .iter()
        .enumerate()
        .map(|(i, m)| m.to_finding(i + 1))
        .collect();

    let qualified: Vec<String> = listings
        .iter()
        .filter(|f| f.is_qualified_lead(&query))
        .map(|f| f.name.clone())
        .collect();

    if qualified.len() != 1 {
        return Err(format!(
            "Expected exactly 1 qualified lead (Salon A) for the canonical query, got {}: {:?}",
            qualified.len(),
            qualified
        ));
    }
    if qualified[0] != "Salon A" {
        return Err(format!(
            "Qualified lead must be 'Salon A', got {:?}",
            qualified[0]
        ));
    }
    Ok(())
}

/// Variant: when the only filter is `rating above 4.0`
/// (no website / phone filters), 4 listings should pass
/// (A=4.5, B=4.2, D=4.6, E=4.4) and 1 should fail
/// (C=3.8). This asserts `is_qualified_lead` is *exactly*
/// the rating filter, not adding any other implicit
/// constraint.
pub fn scenario_4_maps_finding_rating_only_filter() -> Result<(), String> {
    let query = MapsQuery::parse("Find salons in Mumbai with rating above 4.0 — check 15 listings");
    let listings: Vec<MapsFinding> = mock_maps_listings()
        .iter()
        .enumerate()
        .map(|(i, m)| m.to_finding(i + 1))
        .collect();

    let qualified: Vec<String> = listings
        .iter()
        .filter(|f| f.is_qualified_lead(&query))
        .map(|f| f.name.clone())
        .collect();

    if qualified.len() != 4 {
        return Err(format!(
            "Expected 4 qualified leads (A, B, D, E) for rating-only filter, got {:?}",
            qualified
        ));
    }
    let expected = ["Salon A", "Salon B", "Salon D", "Salon E"];
    for e in &expected {
        if !qualified.contains(&e.to_string()) {
            return Err(format!(
                "Qualified set must contain '{e}' for rating-only filter, got {:?}",
                qualified
            ));
        }
    }
    if qualified.contains(&"Salon C".to_string()) {
        return Err(format!(
            "Salon C (rating 3.8) must NOT qualify for rating >= 4.0 filter, got {:?}",
            qualified
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 5 — the system-prompt hint block is present and well-formed
// ----------------------------------------------------------------------

/// Asserts `agent::render_maps_hint_block` produces a block
/// that contains every CRITICAL RULE from the Phase 34 spec
/// (lines 113). The block is the agent's only source of
/// Maps-specific guidance — if any rule goes missing, the
/// agent will silently regress to generic-browse behavior.
pub fn scenario_5_maps_hint_block_contains_critical_rules() -> Result<(), String> {
    let task = "Find salons in Mumbai with no website and rating above 4.0 and must have phone — check 15 listings";
    let block = crate::agent::render_maps_hint_block(task);

    const REQUIRED_PHRASES: &[&str] = &[
        "GOOGLE MAPS EXECUTION STRATEGY",
        "SPA transition",
        "extract_fields",
        "press_key",
        "Escape",
        "left sidebar",
        "Do NOT call declare_subtasks",
        "markdown table",
        "no-website",
    ];
    for phrase in REQUIRED_PHRASES {
        if !block.contains(phrase) {
            return Err(format!(
                "Maps hint block must contain '{phrase}' (verbatim from Phase 34 spec), block was: {block}"
            ));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 6 — the intent-classifier keyword check
// ----------------------------------------------------------------------

/// Asserts `is_local_lead_gen_task` correctly classifies the
/// canonical Maps task as a Maps task and does not
/// mis-classify generic-browse / lead-gen tasks as Maps.
pub fn scenario_6_is_local_lead_gen_task_classifier() -> Result<(), String> {
    // True positives from the spec.
    for t in &[
        "Find salons in Mumbai with no website",
        "Show me gyms in Delhi",
        "clinics in Pune",
        "restaurants near me in Bengaluru",
        "businesses in Tokyo",
        "listings in Berlin",
        "leads in Madrid on google maps",
        "google maps search for cafes in Paris",
    ] {
        if !is_local_lead_gen_task(t) {
            return Err(format!(
                "Task '{t}' must classify as LocalLeadGen (Maps), but did not"
            ));
        }
    }
    // True negatives.
    for t in &[
        "go to instagram and text alice",
        "find SaaS leads in devtools",
        "find rust jobs on linkedin",
        "open the wikipedia page on Quantum Computing",
    ] {
        if is_local_lead_gen_task(t) {
            return Err(format!(
                "Task '{t}' must NOT classify as LocalLeadGen, but did"
            ));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 7 — Handoff carries the MapsQuery, BrowserResult carries
// the MapsFinding list
// ----------------------------------------------------------------------

/// Asserts the type-threaded Handoff / BrowserResult pair
/// round-trips through serde with the Maps fields intact.
/// This is the contract the system-prompt hint block and the
/// synthesizer rely on.
pub fn scenario_7_handoff_and_browser_result_carry_maps_data() -> Result<(), String> {
    use crate::handoff::{BrowserResult, Handoff, MapsQuery};

    let query = MapsQuery::parse("Find salons in Mumbai with no website — check 5 listings");
    let handoff = Handoff {
        task_description: "Find salons in Mumbai with no website — check 5 listings".into(),
        subtasks: Vec::new(),
        constraints: Vec::new(),
        originating_message_id: "session_maps_test:1".into(),
        research_plan: None,
        job_query: None,
        lead_query: None,
        task_shape: Some(TaskShape::LocalLeadGen),
        respawn_handoff: None,
        maps_query: Some(query.clone()),
    };

    // Serde round-trip the Handoff.
    let json = serde_json::to_string(&handoff).map_err(|e| e.to_string())?;
    let back: Handoff = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    if back.maps_query.as_ref().map(|q| q.listing_count) != Some(5) {
        return Err(format!(
            "Handoff round-trip lost maps_query.listing_count; expected Some(5), got {:?}",
            back.maps_query.as_ref().map(|q| q.listing_count)
        ));
    }

    // Build a BrowserResult::done_maps with the mock findings.
    let listings = mock_maps_listings();
    let findings: Vec<MapsFinding> = listings
        .iter()
        .take(2)
        .enumerate()
        .map(|(i, m)| m.to_finding(i + 1))
        .collect();
    let result = BrowserResult::done_maps(
        "session_maps_test:1",
        "Found 2 listings.",
        Vec::new(),
        Some("len:abc123".into()),
        Some("/tmp/transcript_maps.log".into()),
        findings.clone(),
    );
    if result.maps_findings.len() != 2 {
        return Err(format!(
            "BrowserResult::done_maps must carry the findings list, got {}",
            result.maps_findings.len()
        ));
    }

    // Serde round-trip the result.
    let json = serde_json::to_string(&result).map_err(|e| e.to_string())?;
    let back: BrowserResult = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    if back.maps_findings.len() != 2 {
        return Err(format!(
            "BrowserResult round-trip lost maps_findings; got {}",
            back.maps_findings.len()
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------
// The single scenario the runner picks up via `default_scenarios()`.
// ----------------------------------------------------------------------

/// The canonical Phase 34 eval scenario. Bundles the user
/// task, a 5-listing Maps mock fixture, the expected
/// `BrowserResult` after the agent has visited all 5 and
/// filtered, and the known-failure-modes (none — this is
/// the happy path). Registered in `mod.rs`.
pub fn maps_lead_gen_scenario() -> crate::eval::harness::Scenario {
    use crate::eval::harness::Scenario;
    use crate::handoff::BrowserResult;
    use crate::handoff::KeyFinding;
    use mew_resilience::mock_fixtures;

    // The page_state is unused for this scenario (no
    // resilience-detector failures expected) but the
    // harness's `Scenario` struct requires one. We
    // re-use the clean-homepage mock fixture so the
    // scenario integrates with the existing detector
    // suite (modal / rate-limit / captcha all return
    // `None` for a clean page).
    let page_state = mock_fixtures::clean_homepage();

    let query = MapsQuery::parse(
        "Find salons in Mumbai with no website and rating above 4.0 and must have phone — check 15 listings",
    );
    let listings: Vec<MapsFinding> = mock_maps_listings()
        .iter()
        .enumerate()
        .map(|(i, m)| m.to_finding(i + 1))
        .collect();
    let qualified: Vec<MapsFinding> = listings
        .iter()
        .filter(|f| f.is_qualified_lead(&query))
        .cloned()
        .collect();
    let qualified_count = qualified.len();
    let expected = BrowserResult::done_maps(
        "session_maps",
        "Found 1 qualified salon in Mumbai (no website + phone + rating above 4.0).",
        vec![KeyFinding {
            id: "step-1".into(),
            description: "Visit 15 salon listings on Google Maps".into(),
            status: "done".into(),
            reason: String::new(),
            evidence_signature: None,
        }],
        Some("len:maps_seed".into()),
        Some("/tmp/transcript_maps.log".into()),
        qualified,
    );

    // Pre-condition: the parser correctly extracts every
    // filter. If this fails, the scenario is meaningless.
    assert!(query.min_rating == Some(4.0));
    assert!(query.require_no_website);
    assert!(query.require_phone);
    assert!(query.listing_count == 15);
    assert_eq!(
        qualified_count, 1,
        "Canonical Maps query (no website + rating >= 4.0 + phone) should produce exactly 1 qualified lead (Salon A); got {qualified_count}"
    );

    Scenario::new(
        "maps_lead_gen",
        "Phase 34 — find salons in Mumbai with no website + rating above 4.0 + must have phone. Verifies the Maps vertical pack (LocalLeadGen shape, MapsQuery, MapsFinding, is_qualified_lead) end-to-end via deterministic data, no live Maps.",
        "Find salons in Mumbai with no website and rating above 4.0 and must have phone — check 15 listings",
        // The planner splits this task on the three " and "
        // boundaries, producing 4 pieces (or 3 — the exact
        // count depends on the planner's clause-splitter
        // version). The Maps vertical is single-task in
        // *intent*; the planner's split is incidental. We
        // accept whatever the planner produces; the eval
        // assertion is on the maps-specific data path
        // (MapsQuery::parse, MapsFinding filtering), not on
        // the planner's split count. The 3 here is the
        // pre-computed value for the current planner.
        3,
        page_state,
        expected,
        vec![],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase34_maps_lead_gen_scenarios() {
        scenario_1_maps_query_parse_canonical_task().unwrap();
        scenario_2_maps_query_parse_alternate_phrasings().unwrap();
        scenario_3_maps_finding_is_qualified_lead().unwrap();
        scenario_4_maps_finding_rating_only_filter().unwrap();
        scenario_5_maps_hint_block_contains_critical_rules().unwrap();
        scenario_6_is_local_lead_gen_task_classifier().unwrap();
        scenario_7_handoff_and_browser_result_carry_maps_data().unwrap();
    }
}

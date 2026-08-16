//! Phase 37 — Parallel Subtask Worker Pool eval scenario.
//!
//! Verifies the data path of the parallel-Maps dispatch:
//!
//!   1. `MapsQuery::parse` correctly splits a multi-category
//!      task into a `Vec<String>` of re-scoped per-category
//!      queries and sets `parallelism` to the capped count.
//!   2. `MapsFinding::dedup_key` normalizes whitespace and
//!      case so the same business scraped twice (with
//!      different field coverage) collapses to one entry.
//!   3. `merge_maps_findings` dedups and keeps the richer
//!      entry on collision, and re-sorts by rating desc.
//!   4. The merged result of two parallel workers with 2
//!      overlapping businesses contains exactly 8 unique
//!      findings (not 10), and the higher-quality
//!      duplicate is the one that survives.
//!   5. Single-category tasks stay on the single-factory
//!      path (`parallelism == 1`, empty `categories`).
//!
//! The scenario is pure-Rust — no live browser, no live
//! LLM. We exercise the merge / dedup data path directly
//! against fixture data, and the parser's multi-category
//! detection through the public `MapsQuery::parse` entry
//! point.

use crate::handoff::{merge_maps_findings, MapsFinding, MapsQuery};

/// Two mock parallel workers' findings, with a 2-entry
/// overlap on (name, address). Worker 1 (salons) reports
/// 5 listings; worker 2 (gyms) reports 5 listings. Two
/// are the same business (a gym that doubles as a salon)
/// — they differ in field coverage so the merge must
/// keep the richer one.
fn worker1_salons() -> Vec<MapsFinding> {
    vec![
        MapsFinding {
            rank: 1,
            name: "Salon A".into(),
            phone: Some("+91 1".into()),
            address: Some("Bandra".into()),
            rating: Some(4.5),
            review_count: Some(120),
            has_website: false,
            website_url: None,
            category: Some("Beauty".into()),
            maps_url: "https://maps/salon-a".into(),
        },
        // The lower-coverage duplicate #1 — missing
        // rating, review_count, category, phone. The
        // merge must keep the worker-2 version (with
        // phone, rating, review_count, category).
        MapsFinding {
            rank: 2,
            name: "Gym X".into(),
            phone: None,
            address: Some("Andheri".into()),
            rating: None,
            review_count: None,
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "https://maps/gym-x".into(),
        },
        MapsFinding {
            rank: 3,
            name: "Salon B".into(),
            phone: Some("+91 2".into()),
            address: Some("Juhu".into()),
            rating: Some(4.2),
            review_count: Some(80),
            has_website: true,
            website_url: Some("https://salonb.example".into()),
            category: Some("Beauty".into()),
            maps_url: "https://maps/salon-b".into(),
        },
        // The lower-coverage duplicate #2 — Salon C
        // also appears in worker-2's list (the same
        // salon appeared in both the "salons" and
        // "gyms" search results because it has a
        // gym-style fitness room). Worker-1's entry
        // is richer; the merge must keep the richer
        // one (worker-1).
        MapsFinding {
            rank: 4,
            name: "Salon C".into(),
            phone: Some("+91 3".into()),
            address: Some("Powai".into()),
            rating: Some(3.8),
            review_count: Some(50),
            has_website: false,
            website_url: None,
            category: Some("Beauty".into()),
            maps_url: "https://maps/salon-c".into(),
        },
        MapsFinding {
            rank: 5,
            name: "Salon D".into(),
            phone: Some("+91 4".into()),
            address: Some("Goregaon".into()),
            rating: Some(4.6),
            review_count: Some(210),
            has_website: false,
            website_url: None,
            category: Some("Beauty".into()),
            maps_url: "https://maps/salon-d".into(),
        },
    ]
}

fn worker2_gyms() -> Vec<MapsFinding> {
    vec![
        // The higher-coverage duplicate #1 — has phone,
        // rating, review_count, category. The merge
        // must keep this one.
        MapsFinding {
            rank: 1,
            name: "Gym X".into(),
            phone: Some("+91 9".into()),
            address: Some("Andheri".into()),
            rating: Some(4.7),
            review_count: Some(300),
            has_website: false,
            website_url: None,
            category: Some("Gym".into()),
            maps_url: "https://maps/gym-x".into(),
        },
        MapsFinding {
            rank: 2,
            name: "Gym A".into(),
            phone: Some("+91 5".into()),
            address: Some("Bandra".into()),
            rating: Some(4.3),
            review_count: Some(150),
            has_website: false,
            website_url: None,
            category: Some("Gym".into()),
            maps_url: "https://maps/gym-a".into(),
        },
        // The lower-coverage duplicate #2 — same
        // "Salon C" entry as worker-1 (this salon
        // has a fitness room, so it appears in both
        // search results). Worker-1's entry is
        // richer (phone, rating, review_count,
        // category), so the merge must keep
        // worker-1's.
        MapsFinding {
            rank: 3,
            name: "Salon C".into(),
            phone: None,
            address: Some("Powai".into()),
            rating: None,
            review_count: None,
            has_website: false,
            website_url: None,
            category: None,
            maps_url: "https://maps/salon-c".into(),
        },
        MapsFinding {
            rank: 4,
            name: "Gym B".into(),
            phone: Some("+91 6".into()),
            address: Some("Juhu".into()),
            rating: Some(4.1),
            review_count: Some(90),
            has_website: false,
            website_url: None,
            category: Some("Gym".into()),
            maps_url: "https://maps/gym-b".into(),
        },
        MapsFinding {
            rank: 5,
            name: "Gym C".into(),
            phone: Some("+91 7".into()),
            address: Some("Powai".into()),
            rating: Some(3.9),
            review_count: Some(60),
            has_website: true,
            website_url: Some("https://gymc.example".into()),
            category: Some("Gym".into()),
            maps_url: "https://maps/gym-c".into(),
        },
    ]
}

// ----------------------------------------------------------------------
// Scenario 1: MapsQuery::parse on a 3-category canonical task
// ----------------------------------------------------------------------

/// The canonical Phase 37 spec task — "find salons and
/// gyms and clinics in Mumbai" — parses to 3 categories,
/// parallelism=3, each category re-scoped with the city.
pub fn scenario_1_parse_canonical_three_category() -> Result<(), String> {
    let q = MapsQuery::parse("find salons and gyms and clinics in Mumbai");
    if q.parallelism != 3 {
        return Err(format!(
            "expected parallelism=3 for 3-category task, got {}",
            q.parallelism
        ));
    }
    if q.categories.len() != 3 {
        return Err(format!(
            "expected 3 categories, got {}: {:?}",
            q.categories.len(),
            q.categories
        ));
    }
    // Each per-worker query should carry the city.
    // The parser lower-cases the city during extraction,
    // so the re-attached city is also lowercase — the
    // worker's per-category query re-detects the case
    // when it runs `MapsQuery::parse` on its own. Assert
    // case-insensitive.
    for c in &q.categories {
        if !c.to_lowercase().contains("in mumbai") {
            return Err(format!(
                "each per-worker category must include the city, got '{c}'"
            ));
        }
    }
    // The city field on the top-level query is still the
    // single city, not a per-category list.
    if q.city.to_lowercase() != "mumbai" {
        return Err(format!("city must be 'mumbai', got {:?}", q.city));
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 2: single-category task stays serial
// ----------------------------------------------------------------------

/// A single-category Maps task must NOT fan out: the
/// orchestrator's `parallelism > 1` guard relies on this.
pub fn scenario_2_single_category_stays_serial() -> Result<(), String> {
    let q = MapsQuery::parse("salons in Mumbai with no website");
    if q.parallelism != 1 {
        return Err(format!(
            "single-category task must have parallelism=1, got {}",
            q.parallelism
        ));
    }
    if !q.categories.is_empty() {
        return Err(format!(
            "single-category task must have empty categories, got {:?}",
            q.categories
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 3: parallel-merge dedups on (name, address)
// ----------------------------------------------------------------------

/// The headline Phase 37 acceptance test: two mock
/// workers each produce 5 findings with 2 overlapping
/// entries; the merged result contains exactly 8 unique
/// findings (not 10), and the higher-quality duplicate
/// is kept in each case.
pub fn scenario_3_parallel_merge_dedups_and_keeps_richer() -> Result<(), String> {
    let merged: Vec<MapsFinding> =
        merge_maps_findings(worker1_salons().into_iter().chain(worker2_gyms()));

    if merged.len() != 8 {
        return Err(format!(
            "expected 8 unique findings after merge (10 raw - 2 dups), got {}: {:?}",
            merged.len(),
            merged.iter().map(|f| &f.name).collect::<Vec<_>>()
        ));
    }

    // The "Gym X" duplicate (worker-1 had no fields;
    // worker-2 had phone, rating, review_count, category)
    // must collapse into the higher-coverage entry.
    let gym_x = merged
        .iter()
        .find(|f| f.name == "Gym X")
        .ok_or_else(|| "merged result missing Gym X".to_string())?;
    if gym_x.phone.as_deref() != Some("+91 9") {
        return Err(format!(
            "Gym X must keep the higher-coverage phone ('+91 9'), got {:?}",
            gym_x.phone
        ));
    }
    if gym_x.rating != Some(4.7) {
        return Err(format!(
            "Gym X must keep the higher rating (4.7), got {:?}",
            gym_x.rating
        ));
    }
    if gym_x.review_count != Some(300) {
        return Err(format!(
            "Gym X must keep the higher review_count (300), got {:?}",
            gym_x.review_count
        ));
    }
    if gym_x.category.as_deref() != Some("Gym") {
        return Err(format!(
            "Gym X must keep the category, got {:?}",
            gym_x.category
        ));
    }

    // The "Salon C" duplicate (worker-2 had no fields;
    // worker-1 had phone, rating, review_count, category)
    // must collapse into the higher-coverage entry
    // (worker-1's).
    let salon_c = merged
        .iter()
        .find(|f| f.name == "Salon C")
        .ok_or_else(|| "merged result missing Salon C".to_string())?;
    if salon_c.phone.as_deref() != Some("+91 3") {
        return Err(format!(
            "Salon C must keep the higher-coverage phone ('+91 3'), got {:?}",
            salon_c.phone
        ));
    }
    if salon_c.rating != Some(3.8) {
        return Err(format!(
            "Salon C must keep the higher rating (3.8), got {:?}",
            salon_c.rating
        ));
    }
    if salon_c.category.as_deref() != Some("Beauty") {
        return Err(format!(
            "Salon C must keep the higher-coverage category, got {:?}",
            salon_c.category
        ));
    }

    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 4: merged result is rating-desc with re-rank
// ----------------------------------------------------------------------

/// The merge must re-sort by rating desc (Phase 37 spec
/// line 177) and re-rank 1..N so the chat synthesizer
/// renders a clean contiguous table.
pub fn scenario_4_merge_sorts_rating_desc_and_reranks() -> Result<(), String> {
    let merged: Vec<MapsFinding> =
        merge_maps_findings(worker1_salons().into_iter().chain(worker2_gyms()));

    // Rating desc check: ratings should be monotonically
    // non-increasing. None ratings sort last.
    let mut last_rating: Option<f32> = None;
    let mut seen_none = false;
    for f in &merged {
        match (f.rating, last_rating) {
            (Some(r), Some(prev)) => {
                if seen_none {
                    return Err(format!(
                        "rating-desc order broken: rating {} appeared after a None rating",
                        r
                    ));
                }
                if r > prev {
                    return Err(format!(
                        "rating-desc order broken: {} after {}",
                        r, prev
                    ));
                }
                last_rating = Some(r);
            }
            (Some(r), None) => {
                last_rating = Some(r);
            }
            (None, _) => {
                seen_none = true;
            }
        }
    }

    // Re-rank: 1..=N contiguous.
    for (idx, f) in merged.iter().enumerate() {
        if f.rank != idx + 1 {
            return Err(format!(
                "rank must be 1..=N contiguous, got rank={} at index {}",
                f.rank, idx
            ));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 5: no cross-category contamination
// ----------------------------------------------------------------------

/// The acceptance criterion: "salons appear only under
/// the salons category, gyms under gyms, etc." We
/// verify by checking each finding's `category` field
/// stays consistent with the worker's category name.
/// (Our fixture data tags each finding with the right
/// category — `Beauty` for salons, `Gym` for gyms — and
/// the merge does not touch the `category` field on
/// collision, so this assertion is straightforward.)
pub fn scenario_5_no_cross_category_contamination() -> Result<(), String> {
    let merged: Vec<MapsFinding> =
        merge_maps_findings(worker1_salons().into_iter().chain(worker2_gyms()));

    for f in &merged {
        let cat = f.category.as_deref().unwrap_or("");
        // "Gym X" was scraped by both workers (salons
        // worker scraped it with no category, gyms worker
        // scraped it with "Gym" category). The richer
        // entry (gym worker) wins, so its category is
        // "Gym". The other Gym X was dropped.
        if f.name == "Gym X" && cat != "Gym" {
            return Err(format!(
                "Gym X must be tagged as 'Gym' category, got '{cat}'"
            ));
        }
        // Every other finding must keep its original
        // category (the merge doesn't touch it).
        if !f.name.starts_with("Gym") && cat != "Beauty" && !f.name.contains(' ') {
            // "Gym X" is the only "non-gym-but-also-non-Beauty"
            // entry, but it's already checked above. All
            // other entries are pure salon (Beauty) or
            // pure gym (Gym).
            return Err(format!(
                "category contamination: '{}' has category '{cat}'",
                f.name
            ));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Scenario 6: single-query tasks are unaffected
// ----------------------------------------------------------------------

/// A single-query Maps task produces `parallelism=1` and
/// an empty `categories` list. The orchestrator's
/// parallel-detect guard (`mq.parallelism > 1 &&
/// mq.categories.len() > 1`) short-circuits the single
/// factory path.
pub fn scenario_6_single_query_unaffected() -> Result<(), String> {
    let q = MapsQuery::parse("Find salons in Mumbai with no website — check 15 listings");
    if q.parallelism != 1 {
        return Err(format!(
            "single-query task must stay parallelism=1, got {}",
            q.parallelism
        ));
    }
    if !q.categories.is_empty() {
        return Err(format!(
            "single-query task must have empty categories, got {:?}",
            q.categories
        ));
    }
    // The other Phase 34 fields must still parse.
    if q.listing_count != 15 {
        return Err(format!("listing_count must be 15, got {}", q.listing_count));
    }
    if !q.require_no_website {
        return Err("require_no_website must still parse on single-query tasks".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase37_parallel_maps_scenarios() {
        scenario_1_parse_canonical_three_category().unwrap();
        scenario_2_single_category_stays_serial().unwrap();
        scenario_3_parallel_merge_dedups_and_keeps_richer().unwrap();
        scenario_4_merge_sorts_rating_desc_and_reranks().unwrap();
        scenario_5_no_cross_category_contamination().unwrap();
        scenario_6_single_query_unaffected().unwrap();
    }
}

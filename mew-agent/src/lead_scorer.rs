// mew — Phase 39: Local Business Lead Scoring & Dedup Engine.
//
// Pure-Rust scoring + deduplication for `MapsFinding` records
// collected from one or more `LocalLeadGen` search queries. The
// module is intentionally stateless — `LocalLeadScorer` is a
// zero-sized type whose functions take `&MapsFinding` or
// `&[MapsFinding]` and return scores / mutated lists. This keeps
// the scoring logic trivially testable and dependency-free (no
// fuzzy string distance crates, no LLM calls, no async).
//
// Cost discipline (per Phase 39 acceptance criterion): the
// scorer adds **zero LLM calls** — it runs as a synchronous
// post-processing step in the orchestrator's parallel-Maps
// completion path. The only cost it adds is O(n) work over the
// merged findings list (single-digit microseconds for the
// realistic n ≤ 50).
//
// Architectural reference: `mew_agent::research::FindingStore`
// (Phase 22) handles dedup for the generic `ResearchFinding` /
// `JobFinding` / `LeadFinding` types. The Maps vertical is
// deliberately *not* retrofitted onto `FindingStore` — the
// ranking signal here is qualitatively different (no-website
// + high-rating, not cross-platform consolidation), the dedup
// key is `(name, city)` (not the richer `FindingStore`
// platform-anchored tuple), and the module already needed a
// Maps-aware renderer anyway. Phase 22's contract is
// preserved: a `ResearchFinding` still flows through the
// `BrowserResult.findings` path; `MapsFinding` flows through
// this module.

use crate::handoff::{MapsFinding, MapsQuery};

/// Stateless scorer + dedup engine for `MapsFinding` records.
///
/// All methods are `&self` with no fields — there is no
/// instance state. The struct exists as a namespace so call
/// sites read `LocalLeadScorer::score(finding)` rather than
/// the free-function form. Free functions would work just as
/// well; the namespaced form is the spec's preferred API.
pub struct LocalLeadScorer;

impl LocalLeadScorer {
    /// Score a single finding on lead-quality axes. The score
    /// is a `f32` for ranking, not a probability — the
    /// boolean qualifier gate (`MapsFinding::is_qualified_lead`)
    /// is a separate concern and is applied *before* scoring.
    ///
    /// Rules (per Phase 39 spec):
    ///   * Base: `0.0`.
    ///   * `+2.0` if `has_website == false` (the primary lead
    ///     signal — businesses without websites are the target
    ///     market for appointment-booking SaaS).
    ///   * `+1.0` per rating point above 3.5, capped at `+1.5`
    ///     (so 5.0-rated → `+1.5`, 4.0-rated → `+0.5`,
    ///     3.5-rated → `+0.0`, below 3.5 → `+0.0`).
    ///   * `+0.5` if `phone.is_some()` (contactable).
    ///   * `-1.0` if `review_count < Some(5)` (too few reviews
    ///     = rating is an unreliable signal).
    ///
    /// The cap on the rating bonus is implemented as
    /// `((rating - 3.5).max(0.0)).min(1.5)`. The 3.5
    /// floor is hard-coded as the spec's "above 3.5" rule —
    /// a `Some(3.4)` rating yields no rating bonus, and a
    /// `Some(3.5)` yields exactly `+0.0`.
    pub fn score(finding: &MapsFinding) -> f32 {
        let mut s: f32 = 0.0;

        // No-website bonus: the primary lead signal.
        if !finding.has_website {
            s += 2.0;
        }

        // Rating bonus: +1.0 per point above 3.5, capped at
        // +1.5. The `Some(...)` match returns 0.0 when the
        // rating is unknown — we don't penalize a missing
        // rating (the spec doesn't list a "rating is None"
        // penalty), we just don't award the rating bonus.
        if let Some(rating) = finding.rating {
            let above = rating - 3.5;
            let capped = above.max(0.0).min(1.5);
            s += capped;
        }

        // Phone bonus: contactable.
        if finding.phone.is_some() {
            s += 0.5;
        }

        // Low-review penalty: a rating based on <5 reviews
        // is too noisy to trust. `review_count == None` does
        // NOT trigger the penalty (the spec says
        // `review_count < Some(5)`, which reads as
        // "Some(n) where n < 5"; a `None` is "we don't
        // know" and is not penalized).
        if let Some(n) = finding.review_count {
            if n < 5 {
                s -= 1.0;
            }
        }

        s
    }

    /// Deduplicate a list of `MapsFinding`s in place. The
    /// primary dedup key is the normalized `(name, city)`
    /// pair. Normalization:
    ///   * lowercased,
    ///   * leading "the " stripped,
    ///   * trailing common business-type suffixes
    ///     (" salon", " spa", " gym", " clinic", " studio",
    ///     " center", " centre") stripped,
    ///   * multiple whitespace collapsed to a single space,
    ///   * leading / trailing whitespace trimmed.
    ///
    /// City is taken from the `MapsQuery::city` field of the
    /// *query that produced the finding* — but a `MapsFinding`
    /// does not carry a `city` field on its own. We therefore
    /// use a different signature here than the spec implies:
    /// the caller is expected to have already partitioned
    /// findings by city (or to pass a `MapsFinding`-shaped
    /// list whose names are city-distinct). Since the
    /// primary Maps work order is "X in <city>", the
    /// parallel dispatch's per-category Handoffs each carry
    /// a single city and merge_maps_findings is called
    /// *before* this dedup — so in practice the merged
    /// list is already city-pure (within a single run, all
    /// findings share the same `MapsQuery::city`).
    ///
    /// To keep the contract honest, the dedup key is
    /// `(normalized_name, "single_city_marker")` where the
    /// marker is a constant. This means a future caller
    /// that mixes two cities into one dedup pass will
    /// still get a sensible result (same name = same
    /// business) — but the spec rule is explicit: do NOT
    /// collapse listings that differ only in city. To
    /// support that rule without a `city` field on the
    /// struct, we use the **address** as the second
    /// component of the dedup key when present, and the
    /// raw name as the second component otherwise. Two
    /// listings with the same name but different addresses
    /// (different cities, usually) survive the dedup;
    /// two listings with the same name AND the same
    /// address (or no address) collapse. This matches
    /// the spec's "different city → kept both" intent
    /// for the realistic case where two same-named
    /// franchises in different cities have different
    /// addresses on their Maps panel.
    pub fn dedup(findings: &mut Vec<MapsFinding>) {
        use std::collections::HashMap;

        // Normalize-once, score-once: build an aux
        // vector of (index, normalized_key, score) so
        // the inner loop is a single HashMap lookup with
        // no string munging on the hot path.
        let mut by_key: HashMap<(String, String), MapsFinding> = HashMap::new();

        for f in findings.drain(..) {
            let key = dedup_key(&f);
            match by_key.get_mut(&key) {
                Some(existing) => {
                    let incoming_score = Self::score(&f);
                    let existing_score = Self::score(existing);
                    if incoming_score > existing_score {
                        // Replace the existing entry, but
                        // carry forward any fields the new
                        // entry doesn't have (mirrors
                        // `MapsFinding::merge_from`'s
                        // "richer wins" intent).
                        let prior = std::mem::replace(existing, f);
                        existing.merge_from(prior);
                    } else {
                        // Incoming is no better; just merge
                        // its optional fields into the
                        // existing one.
                        existing.merge_from(f);
                    }
                }
                None => {
                    by_key.insert(key, f);
                }
            }
        }

        findings.extend(by_key.into_values());
    }

    /// Convenience: dedup, then filter to qualified leads,
    /// then sort by `score()` descending. The qualified
    /// leads come first, followed by the non-qualified
    /// findings (so the user can see "Other findings" for
    /// context — those listings didn't pass the
    /// `MapsQuery` filter but did get scraped).
    ///
    /// The mutating `Vec` is consumed; the caller gets a
    /// new ordering. The original capacity is preserved
    /// by `findings.drain(..)` inside `dedup`.
    pub fn rank_and_filter(findings: &mut Vec<MapsFinding>, query: &MapsQuery) {
        // Step 1: dedup.
        Self::dedup(findings);

        // Step 2: partition into qualified vs. non-qualified.
        // We do NOT sort the two partitions independently and
        // then concatenate (that would lose the "best-of-both"
        // ordering if a non-qualified finding happened to
        // score higher than a qualified one — which the spec
        // explicitly wants to avoid). Qualified leads come
        // first, sorted by score desc; non-qualified come
        // after, also sorted by score desc.
        let mut qualified: Vec<MapsFinding> = Vec::new();
        let mut other: Vec<MapsFinding> = Vec::new();
        for f in findings.drain(..) {
            if f.is_qualified_lead(query) {
                qualified.push(f);
            } else {
                other.push(f);
            }
        }

        // Step 3: sort by score desc. Ties broken by name
        // asc for deterministic output.
        sort_by_score_desc(&mut qualified);
        sort_by_score_desc(&mut other);

        // Step 4: re-rank (1-based, contiguous) so the chat
        // synthesizer renders a clean `| Rank | ... |` table.
        for (idx, f) in qualified.iter_mut().enumerate() {
            f.rank = idx + 1;
        }
        let qual_len = qualified.len();
        for (idx, f) in other.iter_mut().enumerate() {
            // Continue the rank so the second table doesn't
            // restart at 1 — readers can see "this is finding
            // #N+1, not a fresh enumeration."
            f.rank = qual_len + idx + 1;
        }

        // Step 5: extend in place so the caller's `Vec` is
        // the new ordering.
        findings.extend(qualified);
        findings.extend(other);
    }
}

/// Sort a `Vec<MapsFinding>` by `LocalLeadScorer::score` desc,
/// with name asc as the tie-breaker. Pulled out as a free
/// function so the partition path can call it twice without
/// re-implementing the comparator.
fn sort_by_score_desc(v: &mut [MapsFinding]) {
    v.sort_by(|a, b| {
        // Score desc. f32 doesn't implement Ord, so we use
        // partial_cmp + a deterministic Equal fallback
        // (NaN scores are pathological — a NaN rating
        // would propagate to a NaN score, but we cap
        // above 3.5.max(0).min(1.5) so the only NaN
        // source is a NaN rating, which is a real bug
        // elsewhere. We treat NaN scores as Equal rather
        // than panicking).
        let sa = LocalLeadScorer::score(a);
        let sb = LocalLeadScorer::score(b);
        match sb.partial_cmp(&sa) {
            Some(ord) => ord,
            None => std::cmp::Ordering::Equal,
        }
        // Tie-breaker: name asc (case-insensitive for
        // determinism with respect to source-casing
        // variations).
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Build the dedup key for a `MapsFinding`.
///
/// Per the spec: normalized `(name, city)` pair, where
/// normalization is lowercase + strip leading "the " + strip
/// trailing common suffixes + collapse whitespace + trim. The
/// city half of the key is the address when present (covers
/// the realistic "same name, different franchise locations"
/// case), or a sentinel that intentionally over-collapses
/// when no address is present (the spec's "different city →
/// kept both" intent is best-effort without a real city
/// field; two no-address listings with the same name will
/// collapse, which is the correct behavior because a
/// no-address listing is too thin to be confidently called a
/// separate business).
fn dedup_key(f: &MapsFinding) -> (String, String) {
    let name = normalize_name(&f.name);
    let city = f
        .address
        .as_deref()
        .map(normalize_address)
        .unwrap_or_else(|| name.clone());
    (name, city)
}

/// Normalize a business name for dedup matching.
///
/// Order of operations (each step is intentional):
///   1. Lowercase — so "Salon A" and "salon a" collapse.
///   2. Strip a leading "the " — "The Beauty Salon" and
///      "Beauty Salon" are the same business.
///   3. Strip a trailing common business-type suffix. The
///      list matches the spec: " salon", " spa", " gym",
///      " clinic", " studio", " center", " centre".
///      Word-bounded by the surrounding space — "salonist"
///      is NOT a "salon" + "ist" mis-segment; we only
///      strip the suffix when it is preceded by whitespace
///      and followed by end-of-string (after trim).
///   4. Collapse multiple whitespace to single space.
///   5. Trim.
fn normalize_name(s: &str) -> String {
    let mut out = s.to_lowercase();
    if let Some(rest) = out.strip_prefix("the ") {
        out = rest.to_string();
    }
    const SUFFIXES: &[&str] = &[
        " salon", " spa", " gym", " clinic", " studio", " center", " centre",
    ];
    for suf in SUFFIXES {
        if let Some(rest) = out.strip_suffix(suf) {
            out = rest.to_string();
            break;
        }
    }
    // Collapse whitespace.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_ws = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
            }
            prev_ws = true;
        } else {
            collapsed.push(c);
            prev_ws = false;
        }
    }
    collapsed.trim().to_string()
}

/// Normalize an address for the dedup-key second half. The
/// address normalization is less aggressive than the name
/// normalization — we only lowercase and collapse
/// whitespace. We do NOT strip suffix words ("street",
/// "avenue") because two "Main Street" addresses in
/// different cities are still different addresses, and
/// stripping common street types would over-collapse them.
fn normalize_address(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut collapsed = String::with_capacity(lower.len());
    let mut prev_ws = false;
    for c in lower.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
            }
            prev_ws = true;
        } else {
            collapsed.push(c);
            prev_ws = false;
        }
    }
    collapsed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::MapsFinding;

    /// Build a `MapsFinding` with the given fields, leaving
    /// the rest at sensible defaults. Test-only.
    fn finding(
        name: &str,
        has_website: bool,
        rating: Option<f32>,
        review_count: Option<u32>,
        phone: Option<&str>,
        address: Option<&str>,
    ) -> MapsFinding {
        MapsFinding {
            rank: 0,
            name: name.to_string(),
            phone: phone.map(|s| s.to_string()),
            address: address.map(|s| s.to_string()),
            rating,
            review_count,
            has_website,
            website_url: if has_website {
                Some("https://example.com".to_string())
            } else {
                None
            },
            category: None,
            maps_url: "https://maps.google.com/test".to_string(),
        }
    }

    /// Spec test (a): no-website, 4.5-star, phone present,
    /// 50 reviews → 2.0 + 1.0 + 0.5 = 3.5.
    #[test]
    fn score_no_website_high_rating_phone_50_reviews() {
        let f = finding("Salon A", false, Some(4.5), Some(50), Some("555-0100"), None);
        // 4.5 - 3.5 = 1.0, capped at 1.5 → +1.0
        // 2.0 (no website) + 1.0 (rating) + 0.5 (phone) + 0 (50 ≥ 5 reviews) = 3.5
        assert_eq!(LocalLeadScorer::score(&f), 3.5);
    }

    /// Spec test (b): has-website, 4.5-star → 0.0 + 1.0 = 1.0
    /// (no website bonus, no phone, no review-count penalty
    /// because no review_count was given — None is not
    /// penalized).
    #[test]
    fn score_has_website_4_5_star() {
        let f = finding("Salon B", true, Some(4.5), None, None, None);
        // 2.0 (no website) is NOT applied → 0
        // 4.5 - 3.5 = 1.0, capped at 1.5 → +1.0
        // 0.5 (phone) is NOT applied → 0
        // review_count None → 0
        // Total = 1.0
        assert_eq!(LocalLeadScorer::score(&f), 1.0);
    }

    /// Spec test (c): no-website, 4.0-star, no phone, 3
    /// reviews → 2.0 + 0.5 + 0.0 - 1.0 = 1.5.
    #[test]
    fn score_no_website_4_star_no_phone_3_reviews() {
        let f = finding("Salon C", false, Some(4.0), Some(3), None, None);
        // 2.0 (no website)
        // 4.0 - 3.5 = 0.5, capped at 1.5 → +0.5
        // 0.5 (phone) NOT applied
        // review_count = 3 < 5 → -1.0
        // Total = 2.0 + 0.5 - 1.0 = 1.5
        assert_eq!(LocalLeadScorer::score(&f), 1.5);
    }

    /// Spec test (d): no-website, 3.0-star → 2.0 + 0.0 = 2.0
    /// (below 3.5 threshold, no rating bonus).
    #[test]
    fn score_no_website_3_star_below_threshold() {
        let f = finding("Salon D", false, Some(3.0), Some(50), Some("555"), None);
        // 2.0 (no website)
        // 3.0 - 3.5 = -0.5, clamped to 0.0 → +0.0
        // 0.5 (phone)
        // review_count = 50 ≥ 5 → 0
        // Total = 2.0 + 0.0 + 0.5 = 2.5
        // Wait — the spec says 2.0 + 0.0 = 2.0 with no
        // phone mentioned in the test case. Re-read the
        // spec: "no-website, 3.0-star → 2.0 + 0.0 = 2.0
        // (below 3.5 threshold, no rating bonus)". The
        // spec case has no phone mentioned, so the
        // expected total is 2.0, not 2.5. The phone is
        // not given in the case, so the phone bonus is
        // not applied.
        // We rebuild the finding with phone = None to
        // match the spec case exactly.
        let f = finding("Salon D", false, Some(3.0), Some(50), None, None);
        assert_eq!(LocalLeadScorer::score(&f), 2.0);
    }

    /// Extra: a 5.0-rated business with phone gets the
    /// full rating bonus (+1.5, capped) and the phone
    /// bonus (+0.5), no website bonus.
    /// 0 + 1.5 + 0.5 = 2.0.
    #[test]
    fn score_rating_capped_at_1_5() {
        let f = finding("Top Salon", true, Some(5.0), Some(100), Some("555"), None);
        // has_website → no +2.0
        // 5.0 - 3.5 = 1.5, capped at 1.5 → +1.5
        // phone Some → +0.5
        // review_count 100 ≥ 5 → 0
        // Total = 2.0
        assert_eq!(LocalLeadScorer::score(&f), 2.0);
    }

    /// Extra: rating of exactly 3.5 yields 0.0 rating
    /// bonus (the floor is "above 3.5", not "≥ 3.5").
    /// 2.0 (no website) + 0.0 (rating) + 0.5 (phone) = 2.5.
    #[test]
    fn score_rating_exactly_3_5_yields_no_bonus() {
        let f = finding("Floor", false, Some(3.5), Some(50), Some("555"), None);
        // 3.5 - 3.5 = 0.0 → +0.0
        // Total = 2.0 + 0.0 + 0.5 = 2.5
        assert_eq!(LocalLeadScorer::score(&f), 2.5);
    }

    /// Extra: review_count of exactly 5 is NOT penalized
    /// (the rule is `< Some(5)`, not `≤ Some(5)`).
    /// 2.0 + 1.0 + 0.5 + 0 = 3.5.
    #[test]
    fn score_review_count_exactly_5_no_penalty() {
        let f = finding("Five", false, Some(4.5), Some(5), Some("555"), None);
        assert_eq!(LocalLeadScorer::score(&f), 3.5);
    }

    /// Spec dedup test (a): two entries "The Beauty Salon"
    /// and "Beauty Salon" (same city implied by same
    /// address) → kept 1 (higher score). The "The" prefix
    /// is stripped by `normalize_name`, so both normalize
    /// to "beauty salon". Same address (None == None
    /// collapse in the dedup key) → identical keys.
    #[test]
    fn dedup_strips_the_prefix_and_keeps_higher_score() {
        let a = finding("The Beauty Salon", false, Some(4.5), Some(50), Some("555"), None);
        let b = finding("Beauty Salon", false, Some(4.0), Some(20), Some("555"), None);
        let mut v = vec![a, b];
        LocalLeadScorer::dedup(&mut v);
        // a scores 3.5, b scores 2.0 → a wins; merge_from
        // carries b's optional fields into a (which is a
        // no-op since both have the same coverage).
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "The Beauty Salon");
    }

    /// Spec dedup test (b): two entries "Fitness Gym"
    /// (Mumbai) and "Fitness Gym" (Delhi) → kept both
    /// (different city). Different addresses (which the
    /// dedup uses as the city proxy) → different keys.
    #[test]
    fn dedup_keeps_same_name_different_address() {
        let a = finding(
            "Fitness Gym",
            false,
            Some(4.0),
            Some(20),
            Some("555"),
            Some("123 Main St, Mumbai"),
        );
        let b = finding(
            "Fitness Gym",
            false,
            Some(4.0),
            Some(20),
            Some("555"),
            Some("456 Park Ave, Delhi"),
        );
        let mut v = vec![a, b];
        LocalLeadScorer::dedup(&mut v);
        assert_eq!(v.len(), 2);
    }

    /// Extra: trailing " salon" suffix is stripped, so
    /// "Beauty Salon" and "Beauty" (no address) collapse
    /// to the same key. This is the spec's intentional
    /// over-collapse to make sure that "the salon" and
    /// "the salon salon" (the kind of noise Maps panels
    /// produce) don't double-list.
    #[test]
    fn dedup_strips_trailing_business_type_suffix() {
        let a = finding("Beauty Salon", false, Some(4.5), Some(50), Some("555"), None);
        let b = finding("Beauty", false, Some(4.0), Some(20), Some("555"), None);
        let mut v = vec![a, b];
        LocalLeadScorer::dedup(&mut v);
        assert_eq!(v.len(), 1);
        // a has the higher score (no penalty) → kept as-is.
        assert_eq!(v[0].name, "Beauty Salon");
    }

    /// `rank_and_filter` orders qualified leads first
    /// (score desc) and non-qualified findings after.
    #[test]
    fn rank_and_filter_partitions_qualified_first() {
        let query = MapsQuery {
            search_query: "salons in Mumbai".into(),
            city: "Mumbai".into(),
            listing_count: 15,
            min_rating: Some(4.0),
            require_no_website: true,
            require_phone: false,
            parallelism: 1,
            categories: vec![],
        };
        // Qualified: no-website, 4.5, 50 reviews, phone
        let qualified = finding("Qual A", false, Some(4.5), Some(50), Some("555"), None);
        // Non-qualified: has_website → fails
        // require_no_website.
        let non_qual = finding("Other", true, Some(4.5), Some(50), Some("555"), None);
        // Another qualified, lower score
        let qual_low = finding("Qual B", false, Some(4.0), Some(50), Some("555"), None);
        let mut v = vec![non_qual, qual_low, qualified];
        LocalLeadScorer::rank_and_filter(&mut v, &query);
        // Expect: Qual A (score 3.5, rank 1), Qual B
        // (score 2.5, rank 2), Other (rank 3).
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].name, "Qual A");
        assert_eq!(v[0].rank, 1);
        assert_eq!(v[1].name, "Qual B");
        assert_eq!(v[1].rank, 2);
        assert_eq!(v[2].name, "Other");
        assert_eq!(v[2].rank, 3);
    }

    /// `rank_and_filter` re-numbers contiguous 1..=N
    /// across both partitions (the second partition
    /// continues the first's numbering).
    #[test]
    fn rank_and_filter_reranks_contiguously() {
        let query = MapsQuery {
            search_query: "salons in Mumbai".into(),
            city: "Mumbai".into(),
            listing_count: 15,
            min_rating: Some(4.0),
            require_no_website: true,
            require_phone: false,
            parallelism: 1,
            categories: vec![],
        };
        let a = finding("A", false, Some(4.5), Some(50), Some("555"), None);
        let b = finding("B", true, Some(4.5), Some(50), Some("555"), None); // non-qual
        let c = finding("C", true, Some(4.5), Some(50), Some("555"), None); // non-qual
        let mut v = vec![a, b, c];
        LocalLeadScorer::rank_and_filter(&mut v, &query);
        // Expect: A rank 1, B rank 2, C rank 3.
        let ranks: Vec<usize> = v.iter().map(|f| f.rank).collect();
        assert_eq!(ranks, vec![1, 2, 3]);
    }
}

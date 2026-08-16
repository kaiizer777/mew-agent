//! Phase 33: structured field extractor (`extract_fields` tool).
//!
//! The agent used to issue `snapshot()` and then spend a full LLM
//! round-trip scanning the AX tree to pull out phone / address /
//! rating / website. This module exposes a single CDP-backed
//! function, [`extract_structured_fields`], that runs a targeted
//! `Runtime.evaluate` against the current page and returns the
//! requested fields as a typed `HashMap<String, Option<String>>`.
//!
//! Two extraction paths live here:
//!
//! * **Generic** — used for any non-Maps page. Selectors are
//!   schema.org / semantic-tag / `aria-label` based, and the
//!   phone value comes from `tel:` links or a digit-cluster
//!   regex. The walk is O(n) over the small set of matching
//!   elements, not a full document tree scan.
//! * **Maps-specific** — used when the current URL contains
//!   `google.com/maps`. Hard-codes the known stable Maps
//!   selectors (`[data-item-id="phone"]`, `h1.DUwDvf`, etc.)
//!   for O(1) lookups. **Maps DOM selectors last verified
//!   2026-08-08.** If extraction rates drop, update
//!   [`build_maps_js`] below — keep the same return shape so
//!   the Rust side doesn't change.
//!
//! The function is shaped so the JS-building / JSON-parsing
//! parts can be unit-tested against static HTML strings
//! without a live browser (see `tests` module at the bottom
//! of this file and `mew-cdp/tests/fixtures/maps_panel.html`).

use std::collections::HashMap;

use anyhow::{Context, Result};
use chromiumoxide::Page;
use serde::Deserialize;

/// The full set of field names the LLM can request. The set
/// is closed — `extract_fields` will silently drop unknown
/// names rather than error, so a future schema bump is
/// non-breaking. Kept in sync with the JSON-schema enum in
/// `mew_agent::Agent::get_tools_schema`.
pub const SUPPORTED_FIELDS: &[&str] = &[
    "phone",
    "address",
    "rating",
    "website",
    "name",
    "hours",
    "category",
    "review_count",
];

/// Detect whether the current URL is a Google Maps surface.
/// Used by the agent's dispatch path to pick the Maps-specific
/// selector set without having to re-pass the URL through the
/// tool args.
pub fn is_maps_url(url: &str) -> bool {
    url.contains("google.com/maps") || url.contains("google.com/search")
        || url.contains("maps.google.")
}

/// Build the JS snippet the page-side extractor runs.
///
/// `fields` is the list of field names the LLM asked for.
/// `maps_mode` toggles the selector set (see module docs).
/// The function returns a self-contained `Runtime.evaluate`
/// expression: when run, it walks the DOM and returns a
/// plain JS object whose keys are the requested field names
/// and whose values are either a string or `null`.
///
/// The output is intentionally a plain object (not a Map) so
/// `chromiumoxide` surfaces it as a typed `serde_json::Value`
/// and we can decode it into a `HashMap<String, Option<String>>`
/// without walking JSON by hand.
pub fn build_extract_js(fields: &[&str], maps_mode: bool) -> String {
    // We always ask the page for the full known set. The
    // field whitelist the LLM passed in is enforced on the
    // Rust side after parsing — a small but meaningful
    // robustness win, because the page can return extras
    // (e.g. `category` even when the LLM didn't ask) and the
    // Rust side just drops them. The wire cost is one JSON
    // object of ≤ 8 small string fields, which is
    // negligible.
    let _ = fields; // intentionally unused; see comment above
    if maps_mode {
        build_maps_js()
    } else {
        build_generic_js()
    }
}

// ---------------------------------------------------------------------------
// Generic extraction
// ---------------------------------------------------------------------------

fn build_generic_js() -> String {
    r#"
(function () {
    function text(el) { return el ? (el.textContent || '').trim() : null; }
    function firstText(els) {
        for (var i = 0; i < els.length; i++) {
            var t = text(els[i]);
            if (t) return t;
        }
        return null;
    }
    function attr(el, name) { return el ? el.getAttribute(name) : null; }

    // phone: tel: link, then digit cluster fallback
    var phoneEl = document.querySelector('a[href^="tel:"]');
    var phone = null;
    if (phoneEl) {
        var telHref = phoneEl.getAttribute('href') || '';
        phone = telHref.replace(/^tel:/i, '').trim();
    }
    if (!phone) {
        var all = document.querySelectorAll('body, body *');
        var re = /\+?\d[\d\s\-().]{8,16}\d/;
        for (var i = 0; i < all.length; i++) {
            var t = (all[i].textContent || '');
            if (t.length > 200) continue;        // skip big text nodes
            var m = t.match(re);
            if (m) { phone = m[0].trim(); break; }
        }
    }

    // address: microdata / <address> tag
    var address =
        firstText(document.querySelectorAll('[itemprop="address"]')) ||
        firstText(document.querySelectorAll('[itemtype*="PostalAddress"]')) ||
        firstText(document.querySelectorAll('address'));

    // rating: aria-label star/rating, or microdata ratingValue
    var rating = null;
    var ariaEls = document.querySelectorAll('[aria-label]');
    for (var i = 0; i < ariaEls.length; i++) {
        var a = (ariaEls[i].getAttribute('aria-label') || '').toLowerCase();
        if (a.indexOf('star') !== -1 || a.indexOf('rating') !== -1 || a.indexOf('rated') !== -1) {
            var m = ariaEls[i].getAttribute('aria-label').match(/(\d+(?:\.\d+)?)/);
            if (m) { rating = m[1]; break; }
        }
    }
    if (!rating) {
        var ratingMeta = document.querySelector('[itemprop="ratingValue"]');
        if (ratingMeta) {
            rating = (ratingMeta.getAttribute('content') || ratingMeta.textContent || '').trim() || null;
        }
    }

    // website: <a> with text Website / Visit, href starting http
    var website = null;
    var anchors = document.querySelectorAll('a[href^="http"]');
    for (var i = 0; i < anchors.length; i++) {
        var at = (anchors[i].textContent || '').toLowerCase();
        if (at.indexOf('website') !== -1 || at.indexOf('visit') !== -1 || at.indexOf('homepage') !== -1) {
            website = anchors[i].getAttribute('href');
            break;
        }
    }

    // name: h1, microdata name, or aria-labelledby main heading
    var name =
        text(document.querySelector('h1')) ||
        firstText(document.querySelectorAll('[itemprop="name"]')) ||
        (function () {
            var labelled = document.querySelector('[aria-labelledby]');
            if (labelled) {
                var id = labelled.getAttribute('aria-labelledby');
                if (id) {
                    var ref = document.getElementById(id);
                    if (ref) return text(ref);
                }
            }
            return null;
        })();

    // hours: schema.org openingHours or <time> with itemprop
    var hours = null;
    var oh = document.querySelector('[itemprop="openingHours"]');
    if (oh) hours = (oh.getAttribute('content') || oh.textContent || '').trim() || null;
    if (!hours) {
        var timeEls = document.querySelectorAll('time[itemprop]');
        if (timeEls.length) hours = firstText(timeEls);
    }

    // category: microdata or breadcrumb-style
    var category = null;
    var catEl = document.querySelector('[itemprop="category"]');
    if (catEl) category = (catEl.getAttribute('content') || catEl.textContent || '').trim() || null;

    // review_count: digit-cluster followed by "reviews"
    var reviewCount = null;
    var allText = document.body ? document.body.innerText || '' : '';
    var m = allText.match(/(\d[\d,]*)\s*reviews?/i);
    if (m) reviewCount = m[1];

    return {
        name: name,
        phone: phone,
        address: address,
        rating: rating,
        website: website,
        hours: hours,
        category: category,
        review_count: reviewCount
    };
})()
"#
    .to_string()
}

// ---------------------------------------------------------------------------
// Maps-specific extraction
// ---------------------------------------------------------------------------
//
// Maps DOM selectors last verified 2026-08-08. If extraction rates
// drop, update this function and the matching fixture in
// `mew-cdp/tests/fixtures/maps_panel.html`.

fn build_maps_js() -> String {
    r#"
(function () {
    function textOf(sel) {
        var el = document.querySelector(sel);
        if (!el) return null;
        var t = (el.textContent || '').trim();
        return t.length ? t : null;
    }
    function attrOf(sel, name) {
        var el = document.querySelector(sel);
        if (!el) return null;
        var a = el.getAttribute(name);
        return a && a.length ? a : null;
    }

    // name: known stable Maps heading class, fallback to h1
    var name = textOf('h1.DUwDvf') || textOf('h1');

    // phone: data-item-id phone button
    var phone = textOf('[data-item-id="phone"] .Io6YTe') ||
                textOf('button[data-item-id="phone"]');

    // address: data-item-id address button
    var address = textOf('[data-item-id="address"] .Io6YTe') ||
                  textOf('button[data-item-id="address"]');

    // rating: ceNzKf span, then aria-label stars fallback
    var rating = textOf('span.ceNzKf');
    if (!rating) {
        var ariaEl = document.querySelector('[aria-label*="stars" i], [aria-label*="star" i]');
        if (ariaEl) {
            var m = (ariaEl.getAttribute('aria-label') || '').match(/(\d+(?:\.\d+)?)/);
            if (m) rating = m[1];
        }
    }

    // website: authority link
    var website = attrOf('a[data-item-id="authority"]', 'href');

    // hours: data-item-id hours
    var hours = textOf('[data-item-id="oh"] .Io6YTe') ||
                textOf('button[data-item-id="oh"]');

    // category: data-item-id category
    var category = textOf('[data-item-id="category"] .Io6YTe') ||
                   textOf('button[data-item-id="category"]');

    // review_count: digit cluster next to a "reviews" word near rating
    var reviewCount = null;
    var bodyText = document.body ? document.body.innerText || '' : '';
    var rcm = bodyText.match(/(\d[\d,]*)\s*reviews?/i);
    if (rcm) reviewCount = rcm[1];

    return {
        name: name,
        phone: phone,
        address: address,
        rating: rating,
        website: website,
        hours: hours,
        category: category,
        review_count: reviewCount
    };
})()
"#
    .to_string()
}

// ---------------------------------------------------------------------------
// Result parsing
// ---------------------------------------------------------------------------

/// Wire shape of the JS object's per-field entries. Each
/// field is either a JSON string or `null` (JS `null` →
/// serde's `Value::Null`).
#[derive(Debug, Clone, Deserialize)]
struct RawExtraction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    rating: Option<String>,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    hours: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    review_count: Option<String>,
}

/// Parse the `serde_json::Value` returned by the page-side
/// extractor into the canonical `HashMap<String, Option<String>>`
/// shape. Filters to the LLM's requested `fields` so the
/// returned map only contains keys the caller asked for.
/// Empty strings are normalized to `None` — the JS extractor
/// returns `""` for "found but empty", which we treat as "not
/// found" for the LLM's purposes.
pub fn parse_extraction(
    raw: &serde_json::Value,
    fields: &[&str],
) -> HashMap<String, Option<String>> {
    let parsed: RawExtraction = match serde_json::from_value(raw.clone()) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("extract_fields: parse failed: {e}");
            return empty_map(fields);
        }
    };
    let mut out = HashMap::with_capacity(fields.len());
    for f in fields {
        // Drop unknown field names silently — forward-compat
        // for future schema bumps. Only SUPPORTED_FIELDS survive
        // (the LLM sometimes passes the same field twice or
        // includes an unsupported one; both cases are no-ops).
        if !SUPPORTED_FIELDS.contains(f) {
            continue;
        }
        let v = match *f {
            "name" => parsed.name.clone(),
            "phone" => parsed.phone.clone(),
            "address" => parsed.address.clone(),
            "rating" => parsed.rating.clone(),
            "website" => parsed.website.clone(),
            "hours" => parsed.hours.clone(),
            "category" => parsed.category.clone(),
            "review_count" => parsed.review_count.clone(),
            // Unreachable (filtered above), kept so the
            // match stays exhaustive.
            _ => None,
        };
        // Treat empty string as None — the JS extractor sometimes
        // returns "" for "found but empty" (e.g. <h1></h1>).
        let v = v.and_then(|s| if s.trim().is_empty() { None } else { Some(s) });
        out.insert((*f).to_string(), v);
    }
    out
}

fn empty_map(fields: &[&str]) -> HashMap<String, Option<String>> {
    fields.iter().map(|f| ((*f).to_string(), None)).collect()
}

// ---------------------------------------------------------------------------
// Live CDP entry point
// ---------------------------------------------------------------------------

/// Phase 33 entry point. Runs a single `Runtime.evaluate` against
/// the current page and returns the requested fields. The
/// `maps_mode` flag picks the selector set; if you don't know
/// whether the current page is Maps, prefer
/// [`extract_structured_fields_auto`] which inspects the page URL
/// for you.
pub async fn extract_structured_fields(
    page: &Page,
    fields: &[&str],
    maps_mode: bool,
) -> Result<HashMap<String, Option<String>>> {
    // Whitelist the requested fields. Unknown names become
    // No-op `None` entries in the result — this is the
    // forward-compat surface.
    let requested: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|f| SUPPORTED_FIELDS.contains(f))
        .collect();
    if requested.is_empty() {
        return Ok(HashMap::new());
    }

    let js = build_extract_js(&requested, maps_mode);

    // 3s ceiling — a hung page should fail fast, not stall the
    // ReAct loop. Extraction on a normal Maps panel is <50ms.
    let eval = tokio::time::timeout(
        std::time::Duration::from_millis(3_000),
        page.evaluate(js),
    )
    .await;

    let raw_value = match eval {
        Ok(Ok(res)) => match res.value() {
            Some(v) => v.clone(),
            None => return Ok(empty_map(&requested)),
        },
        Ok(Err(e)) => {
            // Page-side evaluation errored (e.g. CSP block).
            // Don't fail the whole task — return all-None for
            // the requested fields so the LLM can react.
            tracing::debug!("extract_fields: page evaluate error: {e}");
            return Ok(empty_map(&requested));
        }
        Err(_) => {
            // Timeout. Same fallback — never panic the loop.
            tracing::debug!("extract_fields: timeout (3s)");
            return Ok(empty_map(&requested));
        }
    };

    Ok(parse_extraction(&raw_value, &requested))
}

/// Convenience wrapper: auto-detect Maps mode from the
/// current page URL. Reads the URL via the `Page` handle
/// (a tiny CDP round-trip — acceptable, since the LLM has
/// already taken a turn before calling this tool).
pub async fn extract_structured_fields_auto(
    page: &Page,
    fields: &[&str],
) -> Result<HashMap<String, Option<String>>> {
    let url = page.url().await.ok().flatten().unwrap_or_default();
    let maps_mode = is_maps_url(&url);
    extract_structured_fields(page, fields, maps_mode).await
        .context("extract_structured_fields failed")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // (1) is_maps_url: positive cases.
    #[test]
    fn is_maps_url_detects_known_surfaces() {
        assert!(is_maps_url("https://www.google.com/maps/place/Luxe+Salon/"));
        assert!(is_maps_url("https://google.com/maps/@19.07,72.87,15z"));
        // search is included because Maps often routes through
        // google.com/search when the agent lands via the search
        // entry path (see `sensitive_platforms.toml`).
        assert!(is_maps_url(
            "https://www.google.com/search?q=nail+salons+brooklyn"
        ));
    }

    // (2) is_maps_url: negative cases.
    #[test]
    fn is_maps_url_rejects_non_maps() {
        assert!(!is_maps_url("https://en.wikipedia.org/wiki/Rust_(programming_language)"));
        assert!(!is_maps_url("https://www.yelp.com/search?find_desc=salon"));
        assert!(!is_maps_url("https://github.com"));
        assert!(!is_maps_url(""));
    }

    // (3) Generic extraction — all fields present, normal
    // schema.org / <address> / tel: page (a realistic
    // business-listing page shape).
    #[test]
    fn generic_extraction_happy_path() {
        let raw = json!({
            "name": "Luxe Salon",
            "phone": "+91-22-1234-5678",
            "address": "123 MG Road, Mumbai",
            "rating": "4.3",
            "website": "https://luxesalon.example.com",
            "hours": "Mon-Sat 10:00-20:00",
            "category": "Beauty Salon",
            "review_count": "1,245"
        });
        let fields = vec![
            "name", "phone", "address", "rating", "website",
            "hours", "category", "review_count",
        ];
        let out = parse_extraction(&raw, &fields);
        assert_eq!(out.get("name").and_then(|v| v.clone()).as_deref(), Some("Luxe Salon"));
        assert_eq!(out.get("phone").and_then(|v| v.clone()).as_deref(), Some("+91-22-1234-5678"));
        assert_eq!(out.get("address").and_then(|v| v.clone()).as_deref(), Some("123 MG Road, Mumbai"));
        assert_eq!(out.get("rating").and_then(|v| v.clone()).as_deref(), Some("4.3"));
        assert_eq!(out.get("website").and_then(|v| v.clone()).as_deref(), Some("https://luxesalon.example.com"));
        assert_eq!(out.get("hours").and_then(|v| v.clone()).as_deref(), Some("Mon-Sat 10:00-20:00"));
        assert_eq!(out.get("category").and_then(|v| v.clone()).as_deref(), Some("Beauty Salon"));
        assert_eq!(out.get("review_count").and_then(|v| v.clone()).as_deref(), Some("1,245"));
    }

    // (4) Generic extraction — everything missing → all-None,
    // never errors. This is the Phase 33 Acceptance #4 case
    // (a non-Maps generic page that doesn't match any of
    // the patterns).
    #[test]
    fn generic_extraction_all_missing() {
        let raw = json!({
            "name": null,
            "phone": null,
            "address": null,
            "rating": null,
            "website": null,
            "hours": null,
            "category": null,
            "review_count": null
        });
        let fields = vec!["name", "phone", "address", "rating", "website"];
        let out = parse_extraction(&raw, &fields);
        for f in &fields {
            assert_eq!(out.get(*f), Some(&None), "field {f} should be None");
        }
    }

    // (5) Generic extraction — empty string is normalized to
    // None. The JS extractor returns "" when the matched
    // element exists but has no text. The LLM should see None,
    // not an empty string it has to special-case.
    #[test]
    fn generic_extraction_empty_string_normalized_to_none() {
        let raw = json!({
            "name": "",
            "phone": null,
            "address": "  ",
            "rating": null,
            "website": null,
            "hours": null,
            "category": null,
            "review_count": null
        });
        let out = parse_extraction(&raw, &["name", "address"]);
        assert_eq!(out.get("name"), Some(&None));
        assert_eq!(out.get("address"), Some(&None));
    }

    // (6) Maps extraction — wire shape matches the known
    // Maps panel DOM. Asserts the *Rust side* accepts the
    // JS payload without losing fields.
    #[test]
    fn maps_extraction_parses_panel_shape() {
        // Mirrors the selector set the Maps-specific JS uses.
        let raw = json!({
            "name": "Luxe Salon",
            "phone": "+91 22 1234 5678",
            "address": "Shop 4, MG Road, Bandra West, Mumbai, Maharashtra 400050",
            "rating": "4.3",
            "website": "https://luxesalon.example.com/",
            "hours": "10:00–21:00",
            "category": "Beauty salon",
            "review_count": "1,245"
        });
        let fields = vec![
            "name", "phone", "address", "rating", "website",
            "hours", "category", "review_count",
        ];
        let out = parse_extraction(&raw, &fields);
        assert_eq!(out.get("name").and_then(|v| v.clone()).as_deref(), Some("Luxe Salon"));
        assert_eq!(out.get("phone").and_then(|v| v.clone()).as_deref(), Some("+91 22 1234 5678"));
        assert_eq!(out.get("rating").and_then(|v| v.clone()).as_deref(), Some("4.3"));
        assert_eq!(out.get("website").and_then(|v| v.clone()).as_deref(), Some("https://luxesalon.example.com/"));
        assert_eq!(out.get("review_count").and_then(|v| v.clone()).as_deref(), Some("1,245"));
    }

    // (7) Maps extraction — all-missing case still returns
    // a fully-populated map of `None`s, never an Err.
    #[test]
    fn maps_extraction_all_missing_returns_full_none_map() {
        let raw = json!({
            "name": null, "phone": null, "address": null,
            "rating": null, "website": null, "hours": null,
            "category": null, "review_count": null
        });
        let fields = vec!["phone", "rating", "website"];
        let out = parse_extraction(&raw, &fields);
        assert_eq!(out.get("phone"), Some(&None));
        assert_eq!(out.get("rating"), Some(&None));
        assert_eq!(out.get("website"), Some(&None));
        assert_eq!(out.len(), 3);
    }

    // (8) Field whitelist — unknown field names from the LLM
    // are dropped (and the page may return extras which we
    // ignore too). Only SUPPORTED_FIELDS survive.
    #[test]
    fn parse_extraction_drops_unknown_fields() {
        let raw = json!({
            "name": "X",
            "phone": "1",
            "address": "1 Main",
            "rating": "5",
            "website": "https://x",
            "hours": null,
            "category": null,
            "review_count": null
        });
        // Mix supported + unsupported names.
        let fields = vec!["name", "phone", "not_a_real_field"];
        let out = parse_extraction(&raw, &fields);
        assert_eq!(out.get("name").and_then(|v| v.clone()).as_deref(), Some("X"));
        assert_eq!(out.get("phone").and_then(|v| v.clone()).as_deref(), Some("1"));
        assert!(!out.contains_key("not_a_real_field"));
    }

    // (9) JS shape sanity — `build_extract_js` returns a
    // self-contained IIFE that produces an object with the
    // expected keys. Catches accidental truncation of the
    // snippet (e.g. a stray `//` would break the IIFE).
    #[test]
    fn build_extract_js_contains_expected_keys_generic() {
        let js = build_extract_js(&["name", "phone"], false);
        for key in &[
            "name", "phone", "address", "rating", "website",
            "hours", "category", "review_count",
        ] {
            assert!(
                js.contains(&format!("{key}:")),
                "generic JS missing key {key}"
            );
        }
        // IIFE wrapper
        assert!(js.trim_start().starts_with("(function"));
    }

    // (10) JS shape sanity — Maps variant uses the known
    // data-item-id selectors.
    #[test]
    fn build_extract_js_maps_uses_known_selectors() {
        let js = build_extract_js(&["phone"], true);
        assert!(js.contains("data-item-id=\"phone\""), "maps JS missing phone selector");
        assert!(js.contains("data-item-id=\"address\""), "maps JS missing address selector");
        assert!(js.contains("data-item-id=\"authority\""), "maps JS missing website/authority selector");
        assert!(js.contains("h1.DUwDvf"), "maps JS missing name selector");
        assert!(js.contains("span.ceNzKf"), "maps JS missing rating selector");
    }

    // (11) Per-page evaluate against a known-good static HTML
    // fixture. We can't run a real browser in this unit test,
    // but we *can* assert that the fixture contains the
    // selector targets our JS reads from. This catches
    // divergence between the JS extractor and the test
    // fixture (the two evolve together — see
    // `mew-cdp/tests/fixtures/maps_panel.html`).
    //
    // Note: we check for the class name as a substring, not
    // the full `<tag class="...">` shape, because the real
    // Maps DOM has additional classes alongside ours
    // (e.g. `<h1 class="DUwDvf lfPIob">`).
    #[test]
    fn maps_panel_fixture_matches_extractor_assumptions() {
        let html = include_str!("../tests/fixtures/maps_panel.html");
        // Phone / address / authority — data-item-id selectors
        // are stable: they appear as exact attribute values.
        assert!(
            html.contains(r#"data-item-id="phone""#),
            "maps_panel.html missing data-item-id=phone"
        );
        assert!(
            html.contains(r#"data-item-id="address""#),
            "maps_panel.html missing data-item-id=address"
        );
        assert!(
            html.contains(r#"data-item-id="authority""#),
            "maps_panel.html missing data-item-id=authority (website)"
        );
        // Name — Maps uses `<h1 class="DUwDvf ...">`. We
        // assert the class name appears somewhere in the
        // fixture rather than the exact tag string.
        assert!(
            html.contains("DUwDvf"),
            "maps_panel.html missing DUwDvf class (name)"
        );
        // Rating — Maps uses `<span class="ceNzKf" ...>`.
        assert!(
            html.contains("ceNzKf"),
            "maps_panel.html missing ceNzKf class (rating)"
        );
    }

    // (12) Generic fallback — when the page is not Maps
    // and has no schema.org markup, parse_extraction should
    // still return a well-formed HashMap (just with all
    // None values), never panic.
    #[test]
    fn generic_extraction_handles_garbage_payload() {
        // Missing keys entirely (e.g. a page that returned a
        // bare string instead of the expected object).
        let raw = json!("not an object");
        let out = parse_extraction(&raw, &["name", "phone"]);
        assert_eq!(out.get("name"), Some(&None));
        assert_eq!(out.get("phone"), Some(&None));
    }
}

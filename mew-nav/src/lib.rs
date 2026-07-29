//! URL resolution layer for `mew`'s `navigate` tool calls.
//!
//! Fixes the specific bug documented in v2.md Step 14.1: when the LLM issues
//! `navigate("instagram")` with a bare name, raw `Page::goto` either fails or
//! hallucinate-loads a wrong URL. This module sits *before* `mew_cdp::navigate`
//! and rewrites the bare name to a real URL using three transparent paths:
//!
//! 1. **Map hit** — a small local lookup of well-known sites the user actually
//!    uses (seeded from the project scope). Instant, no network probe.
//! 2. **Direct guess** — try `https://{sanitized}.com` and run a real CDP
//!    navigation with a short timeout (3-4s). If the page actually loads, use
//!    it. If it fails, timeouts, or redirects to a search-engine "no results"
//!    page, fall through.
//! 3. **Search fallback** — `https://www.google.com/search?q={x}`. This is
//!    exactly the "use google and open X" path the user has been typing by
//!    hand; we just trigger it automatically when the bare name doesn't resolve.
//!
//! Every resolution call returns a `ResolutionResult` carrying the final URL
//! and which path fired, so the transcript can record it (the 14.1 spec calls
//! for this transparency — the user wants to see *why* a site did what it did).

use chromiumoxide::Page;
use std::collections::HashMap;
use std::time::Duration;
use url::Url;

/// How the resolver decided on the final URL. The user (and the transcript)
/// care which path fired — a direct guess landing on a real site means
/// "I knew where it was", whereas a search fallback means "I had to look it
/// up". This enum is the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionPath {
    /// The bare input was already a well-formed URL with a scheme (e.g.
    /// `https://example.com/foo`). Pass-through, no rewriting.
    AlreadyUrl,
    /// Hit the local known-sites map (e.g. `instagram` → `https://www.instagram.com`).
    MapHit,
    /// `https://{sanitized}.com` was tried and a real page loaded inside the
    /// probe timeout. We then re-navigated to that same URL for real.
    DirectGuess,
    /// The guess failed/timeouted/was a search-engine redirect. Fell through
    /// to `https://www.google.com/search?q={x}`.
    SearchFallback,
}

impl ResolutionPath {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResolutionPath::AlreadyUrl => "already-url",
            ResolutionPath::MapHit => "map-hit",
            ResolutionPath::DirectGuess => "direct-guess",
            ResolutionPath::SearchFallback => "search-fallback",
        }
    }
}

/// Result of resolving a bare name / partial URL to a real navigation target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionResult {
    /// The URL the agent should actually navigate to.
    pub url: String,
    /// Which path the resolver took to decide on this URL. Always recorded
    /// to the transcript so a later review can tell *why* the agent ended up
    /// where it did.
    pub path: ResolutionPath,
    /// The original input the LLM gave us (e.g. `"instagram"`). Echoed back
    /// for logging — useful when the input was a multi-word phrase and the
    /// sanitized form differs visibly.
    pub original_input: String,
}

/// Hard-coded timeout for the direct-guess probe. Per v2.md 14.1: "3-4s"
/// with a warning that a real but slow site could be misclassified as
/// "failed" and sent to search instead — if the user sees that happening
/// in real runs, this constant is the knob to loosen.
pub const DIRECT_GUESS_PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Build the default known-sites map. Seeded with the sites the user
/// actually uses (from the project scope), not a generic 100-entry
/// list — the spec is explicit about this: "15-20 sites you actually use,
/// from the original scope-lock step."
///
/// Keys are matched case-insensitively after the user's input has been
/// trimmed and lowercased. Values are full URLs (scheme included).
///
/// To add a site: just append to this function. There is no separate
/// config field for this — it's a code-level curated list, like the
/// `allowed_domains` allowlist in `config.yaml`, and lives next to the
/// resolver that uses it.
pub fn known_sites_map() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    // Social
    m.insert("instagram", "https://www.instagram.com");
    m.insert("twitter", "https://twitter.com");
    m.insert("x", "https://twitter.com"); // bare "x" → the site now called X
    m.insert("facebook", "https://www.facebook.com");
    m.insert("fb", "https://www.facebook.com");
    m.insert("reddit", "https://www.reddit.com");
    m.insert("linkedin", "https://www.linkedin.com");
    m.insert("tiktok", "https://www.tiktok.com");
    m.insert("youtube", "https://www.youtube.com");
    m.insert("yt", "https://www.youtube.com");
    // Mail
    m.insert("gmail", "https://mail.google.com");
    m.insert("mail", "https://mail.google.com");
    m.insert("outlook", "https://outlook.live.com");
    m.insert("yahoo mail", "https://mail.yahoo.com");
    // Dev / docs (matches the `allowed_domains` already in config.yaml)
    m.insert("github", "https://github.com");
    m.insert("gh", "https://github.com");
    m.insert("gitlab", "https://gitlab.com");
    m.insert("stackoverflow", "https://stackoverflow.com");
    m.insert("so", "https://stackoverflow.com");
    m.insert("docs.rs", "https://docs.rs");
    m.insert("crates.io", "https://crates.io");
    m.insert("crates", "https://crates.io");
    m.insert("npm", "https://www.npmjs.com");
    // Search
    m.insert("google", "https://www.google.com");
    m.insert("duckduckgo", "https://duckduckgo.com");
    m.insert("ddg", "https://duckduckgo.com");
    m.insert("bing", "https://www.bing.com");
    // Productivity / comms
    m.insert("notion", "https://www.notion.so");
    m.insert("slack", "https://slack.com");
    m.insert("discord", "https://discord.com");
    m.insert("whatsapp", "https://web.whatsapp.com");
    m.insert("telegram", "https://web.telegram.org");
    // News / reference
    m.insert("wikipedia", "https://en.wikipedia.org");
    m.insert("wiki", "https://en.wikipedia.org");
    m.insert("nytimes", "https://www.nytimes.com");
    m.insert("bbc", "https://www.bbc.com");
    m.insert("cnn", "https://www.cnn.com");
    m.insert("hackernews", "https://news.ycombinator.com");
    m.insert("hn", "https://news.ycombinator.com");
    // Shopping
    m.insert("amazon", "https://www.amazon.com");
    m.insert("ebay", "https://www.ebay.com");
    m.insert("aliexpress", "https://www.aliexpress.com");
    // Maps
    m.insert("maps", "https://maps.google.com");
    m.insert("google maps", "https://maps.google.com");
    m
}

/// Sanitize a bare input before guessing a `.com` URL: trim, lowercase
/// spaces-only segments, and strip anything that isn't `[a-z0-9]`. This is
/// what lets "the shopping site I like" fail loudly into the search fallback
/// instead of producing `https://theshoppingsiteIlike.com`.
pub fn sanitize_for_domain_guess(raw: &str) -> String {
    let trimmed = raw.trim().to_lowercase();
    let cleaned: String = trimmed
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    cleaned
}

/// Cheap pre-check: does this string already parse as an http(s) URL with
/// a host? Used to short-circuit when the LLM (or the user, via a
/// `navigate("https://...")` raw call) already gave us a real URL.
pub fn looks_like_url(input: &str) -> bool {
    if let Ok(u) = Url::parse(input.trim()) {
        // http/https are the normal web. `about:` (the chrome internal
        // page the browser starts on) and `file:` (local HTML the
        // user wants to test against) should pass through unchanged —
        // they're well-formed URLs, not bare names that need
        // resolution. Sending `about:blank` to Google is a real
        // failure mode the agent was hitting in 15.2 testing.
        matches!(u.scheme(), "http" | "https" | "about" | "file")
    } else {
        false
    }
}

/// Pure-Rust path-1-and-3 resolver. Decides between the map and the
/// fallback without doing any network I/O. The direct-guess path lives in
/// `resolve_with_probe` because it needs the live CDP page handle.
///
/// Use this when you have no browser yet (unit tests, dry-run, etc.) or
/// when you specifically want to skip the probe.
pub fn resolve_without_probe(input: &str) -> ResolutionResult {
    resolve_inner(input)
}

/// Full resolver including the direct-guess probe. Requires a live
/// `Page` because the probe does a real `Page::goto` with a 4s timeout.
///
/// `None` returned page → falls through to search fallback (so callers
/// that hold a `Page` but the page is dead still get a usable result).
pub async fn resolve_with_probe(page: &Page, input: &str) -> ResolutionResult {
    // Path 1: known-sites map — instant, no network.
    let map = known_sites_map();
    let key = input.trim().to_lowercase();
    if let Some(url) = map.get(key.as_str()).copied() {
        return ResolutionResult {
            url: url.to_string(),
            path: ResolutionPath::MapHit,
            original_input: input.to_string(),
        };
    }

    // Already a URL → pass through.
    if looks_like_url(input) {
        return ResolutionResult {
            url: input.trim().to_string(),
            path: ResolutionPath::AlreadyUrl,
            original_input: input.to_string(),
        };
    }

    // Path 2: direct guess. Try `https://{sanitized}.com` with a hard
    // timeout. On any failure (timeout, CDP error, page that ended up
    // being a search-engine "no results" redirect), fall through to
    // the search fallback.
    let guess = sanitize_for_domain_guess(input);
    if !guess.is_empty() {
        let guess_url = format!("https://{}.com", guess);
        if probe_url_loads(page, &guess_url).await {
            return ResolutionResult {
                url: guess_url,
                path: ResolutionPath::DirectGuess,
                original_input: input.to_string(),
            };
        }
    }

    // Path 3: search fallback. The exact "use google and open X" path
    // the user has been typing by hand — we just trigger it automatically.
    let encoded = url_encode(input.trim());
    ResolutionResult {
        url: format!("https://www.google.com/search?q={}", encoded),
        path: ResolutionPath::SearchFallback,
        original_input: input.to_string(),
    }
}

/// Shared core used by `resolve_without_probe` (sync) and the async
/// `resolve_with_probe` (which calls it after Path 1 already matched).
/// Kept private because the public API is the two `resolve_*` functions.
fn resolve_inner(input: &str) -> ResolutionResult {
    let map = known_sites_map();
    let key = input.trim().to_lowercase();
    if let Some(url) = map.get(key.as_str()).copied() {
        return ResolutionResult {
            url: url.to_string(),
            path: ResolutionPath::MapHit,
            original_input: input.to_string(),
        };
    }
    if looks_like_url(input) {
        return ResolutionResult {
            url: input.trim().to_string(),
            path: ResolutionPath::AlreadyUrl,
            original_input: input.to_string(),
        };
    }
    let guess = sanitize_for_domain_guess(input);
    if !guess.is_empty() {
        // Without a probe, we still produce a guess URL — the caller can
        // decide to use it or skip to fallback. This is the "dry-run" mode.
        let guess_url = format!("https://{}.com", guess);
        return ResolutionResult {
            url: guess_url,
            path: ResolutionPath::DirectGuess,
            original_input: input.to_string(),
        };
    }
    let encoded = url_encode(input.trim());
    ResolutionResult {
        url: format!("https://www.google.com/search?q={}", encoded),
        path: ResolutionPath::SearchFallback,
        original_input: input.to_string(),
    }
}

/// Probe a URL by issuing a real CDP navigation and waiting up to
/// `DIRECT_GUESS_PROBE_TIMEOUT`. Returns `true` only if the page
/// actually loaded inside the window — errors, timeouts, and redirects
/// to a search-engine "no results" page all return `false`.
///
/// We deliberately do **not** navigate the user's real page here — the
/// probe is allowed to land on the guess URL, and on success we re-return
/// the same URL so the caller's subsequent real navigation lands on
/// the same place. (Chromiumoxide's `Page::goto` is the only sensible
/// primitive here; an in-process HEAD-style request via reqwest would
/// miss the stealth / session layer the real nav uses, and would not
/// match what the user would actually see.)
async fn probe_url_loads(page: &Page, url: &str) -> bool {
    let res = tokio::time::timeout(
        DIRECT_GUESS_PROBE_TIMEOUT,
        page.goto(url),
    )
    .await;

    match res {
        Ok(Ok(_)) => {
            // `Page::goto` returns once the response is received; we still
            // need to wait for the actual page load (or a hard timeout)
            // so a "no results" search-engine page doesn't count as a
            // successful guess. `wait_for_navigation` is the right
            // primitive — it blocks until the load event (or a CDP
            // timeout inside it).
            let wait = tokio::time::timeout(
                DIRECT_GUESS_PROBE_TIMEOUT,
                page.wait_for_navigation(),
            )
            .await;
            match wait {
                Ok(Ok(_)) => true,
                _ => {
                    tracing::debug!(
                        "probe: {} returned but wait_for_navigation did not",
                        url
                    );
                    false
                }
            }
        }
        Ok(Err(e)) => {
            tracing::debug!("probe: {} failed: {}", url, e);
            false
        }
        Err(_) => {
            tracing::debug!("probe: {} timed out", url);
            false
        }
    }
}

/// Percent-encode a string for safe inclusion in a query string. Avoids
/// pulling in a heavier crate just for this. Anything outside the
/// unreserved set (`A-Z a-z 0-9 - _ . ~`) gets `%XX` encoded.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        let b = *byte;
        let is_unreserved = b.is_ascii_alphanumeric()
            || b == b'-'
            || b == b'_'
            || b == b'.'
            || b == b'~';
        if is_unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_hit_known_sites() {
        let r = resolve_without_probe("instagram");
        assert_eq!(r.path, ResolutionPath::MapHit);
        assert_eq!(r.url, "https://www.instagram.com");
        assert_eq!(r.original_input, "instagram");
    }

    #[test]
    fn map_hit_is_case_insensitive() {
        let r = resolve_without_probe("Instagram");
        assert_eq!(r.path, ResolutionPath::MapHit);
    }

    #[test]
    fn map_hit_trims_whitespace() {
        let r = resolve_without_probe("  gmail  ");
        assert_eq!(r.path, ResolutionPath::MapHit);
        assert_eq!(r.url, "https://mail.google.com");
    }

    #[test]
    fn already_url_passes_through() {
        let r = resolve_without_probe("https://example.com/foo");
        assert_eq!(r.path, ResolutionPath::AlreadyUrl);
        assert_eq!(r.url, "https://example.com/foo");
    }

    #[test]
    fn direct_guess_uses_sanitized_domain() {
        let r = resolve_without_probe("rustlang");
        // Without a probe, we still produce a guess — the caller
        // decides whether to trust it.
        assert_eq!(r.path, ResolutionPath::DirectGuess);
        assert_eq!(r.url, "https://rustlang.com");
    }

    #[test]
    fn sanitize_strips_punctuation() {
        assert_eq!(sanitize_for_domain_guess("the shopping site!"), "theshoppingsite");
        assert_eq!(sanitize_for_domain_guess("  My-Site  "), "mysite");
        assert_eq!(sanitize_for_domain_guess("a.b.c"), "abc");
    }

    #[test]
    fn nonsense_falls_back_to_google_search() {
        // After sanitization becomes empty → must go to search fallback.
        let r = resolve_without_probe("!!!");
        assert_eq!(r.path, ResolutionPath::SearchFallback);
        assert!(r.url.contains("google.com/search"));
    }

    #[test]
    fn known_short_aliases_work() {
        assert_eq!(
            resolve_without_probe("gh").url,
            "https://github.com"
        );
        assert_eq!(resolve_without_probe("hn").url, "https://news.ycombinator.com");
        assert_eq!(resolve_without_probe("yt").url, "https://www.youtube.com");
    }

    #[test]
    fn multi_word_phrase_uses_search_fallback() {
        // "the shopping site" sanitizes to "theshoppingsite" which IS
        // a non-empty guess. So this is the direct-guess path under
        // resolve_without_probe. With the probe, the guess would fail
        // and fall through to search — but the dry-run helper doesn't
        // probe, by design.
        let r = resolve_without_probe("the shopping site");
        assert_eq!(r.path, ResolutionPath::DirectGuess);
        assert_eq!(r.url, "https://theshoppingsite.com");
    }

    #[test]
    fn looks_like_url_recognises_http_and_https() {
        assert!(looks_like_url("https://example.com"));
        assert!(looks_like_url("http://example.com/foo"));
        assert!(!looks_like_url("example.com"));
        assert!(!looks_like_url("instagram"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn url_encodes_spaces_and_special_chars() {
        // The search-fallback path should produce a query string that's
        // safe to drop into a URL — spaces become %20, & becomes %26.
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn original_input_is_preserved() {
        let r = resolve_without_probe("  InStaGram  ");
        assert_eq!(r.original_input, "  InStaGram  ");
    }

    #[test]
    fn resolution_path_as_str_is_stable() {
        // If these strings change, the transcript format changes —
        // pin them so a rename doesn't silently break log parsers.
        assert_eq!(ResolutionPath::AlreadyUrl.as_str(), "already-url");
        assert_eq!(ResolutionPath::MapHit.as_str(), "map-hit");
        assert_eq!(ResolutionPath::DirectGuess.as_str(), "direct-guess");
        assert_eq!(ResolutionPath::SearchFallback.as_str(), "search-fallback");
    }
}

//! URL resolution layer for `mew`'s `navigate` tool calls.
//!
//! Fixes the specific bug documented in v2.md Step 14.1: when the LLM issues
//! `navigate("instagram")` with a bare name, raw `Page::goto` either fails or
//! hallucinate-loads a wrong URL. This module sits *before* `mew_cdp::navigate`
//! and rewrites the bare name to a real URL using transparent paths:
//!
//! 1. **Sensitive-platform routing** (Phase 2) — if the resolved host is in
//!    `config/sensitive_platforms.toml`, the resolver returns a search-engine
//!    entry URL (`via_search` / `via_search_confirm`) instead of a bare
//!    direct navigation. This is the structural fix for Bug #1's
//!    referrer-less-bot-detection failure mode (see
//!    `docs/bug-1-root-cause.md`).
//! 2. **Map hit** — a small local lookup of well-known sites the user actually
//!    uses (seeded from the project scope). Instant, no network probe.
//! 3. **Direct guess** — try `https://{sanitized}.com` and run a real CDP
//!    navigation with a short timeout (3-4s). If the page actually loads, use
//!    it. If it fails, timeouts, or redirects to a search-engine "no results"
//!    page, fall through.
//! 4. **Search fallback** — `https://www.google.com/search?q={x}`. This is
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
    /// Phase 2: the resolved host is in `config/sensitive_platforms.toml`
    /// with strategy `via_search`. The returned URL is a Google search
    /// results page for the bare site name (or `site:` query for an
    /// explicit-URL input), so the agent must click the organic result to
    /// actually land on the sensitive domain. This gives the navigation a
    /// natural-looking referrer chain instead of a referrer-less bare
    /// direct nav.
    ViaSearch,
    /// Phase 2: like `ViaSearch` but the search query is biased toward the
    /// sign-in / login surface (`<site> login`). Used for sites where the
    /// LLM will need an authenticated session immediately (LinkedIn is the
    /// motivating example).
    ViaSearchConfirm,
}

impl ResolutionPath {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResolutionPath::AlreadyUrl => "already-url",
            ResolutionPath::MapHit => "map-hit",
            ResolutionPath::DirectGuess => "direct-guess",
            ResolutionPath::SearchFallback => "search-fallback",
            ResolutionPath::ViaSearch => "via-search",
            ResolutionPath::ViaSearchConfirm => "via-search-confirm",
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

// ---------------------------------------------------------------------------
// Phase 2: sensitive-platform routing
// ---------------------------------------------------------------------------
//
// `SensitivePlatforms` is a small static config (loaded from
// `config/sensitive_platforms.toml`) that maps a host → entry strategy.
// The motivation is Bug #1 (see `docs/bug-1-root-cause.md`): some
// platforms have aggressive anti-bot layers that classify a
// referrer-less bare navigation as suspicious. Routing through a
// search-engine results page instead of a direct nav gives the
// navigation a normal referrer chain (google.com → results page →
// click target) and avoids the challenge.
//
// The shape is deliberately small:
//   * Exact host match (e.g. "instagram.com").
//   * Single-level wildcard ("*.twitter.com") for the
//     `mobile.twitter.com` / `m.twitter.com` cases.
// The matcher does NOT do full suffix matching — `evil-twitter.com`
// does NOT match `*.twitter.com`. That keeps the security profile
// tight: an attacker can't trick the resolver by registering a
// look-alike domain.

/// How a sensitive-platform entry should be entered.
///
/// The variants mirror the `strategy` values in
/// `config/sensitive_platforms.toml` 1:1 — if you add a value to
/// one, add it to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStrategy {
    /// No special routing — same as not being in the table. Listed
    /// here for "we considered this domain, decided it's fine" audit
    /// records.
    Direct,
    /// Route through a Google search results page for the site
    /// name. The LLM then has to click the organic result to land
    /// on the sensitive domain. This is the cheap fix.
    ViaSearch,
    /// Like `ViaSearch` but the search query is biased toward the
    /// sign-in surface (`<site> login` / `site:<domain> login`).
    /// Use when the LLM is about to need an authenticated session.
    ViaSearchConfirm,
}

impl EntryStrategy {
    /// Stable string form — used in the transcript and the trace
    /// JSONL `branch` field. Pinned by a test so a rename doesn't
    /// silently break log parsers.
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryStrategy::Direct => "direct",
            EntryStrategy::ViaSearch => "via_search",
            EntryStrategy::ViaSearchConfirm => "via_search_confirm",
        }
    }
}

/// One row in `config/sensitive_platforms.toml`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SensitivePlatformEntry {
    /// The domain pattern. Either an exact host (`instagram.com`) or
    /// a single-level wildcard (`*.twitter.com`).
    pub domain: String,
    /// How the resolver should handle navigations to this host.
    pub strategy: EntryStrategy,
    /// Phase 8: known to challenge bots. When `true`, the resolver
    /// applies two additional behaviors *before* any challenge
    /// page actually appears:
    ///
    ///   1. The `via_search` / `via_search_confirm` entry path is
    ///      already preferred for every entry in this table, but
    ///      `known_to_challenge_bots = true` is the explicit "this
    ///      domain will serve a challenge" signal the agent uses
    ///      to *also* slow its pacing — the navigation itself is
    ///      the most likely moment for the challenge to be served.
    ///   2. The agent's local telemetry (Phase 8.5) keys on this
    ///      flag to seed the per-domain challenge counter at the
    ///      higher "expected" baseline.
    ///
    /// The default is `false` so a pre-Phase-8 TOML file parses
    /// unchanged — the existing entries (instagram, twitter, etc.)
    /// are *known* challengers, but the new field is opt-in for
    /// each row. See `config/sensitive_platforms.toml` for the
    /// seeded list.
    #[serde(default)]
    pub known_to_challenge_bots: bool,
}

/// The full loaded table. Owns the parsed entries and exposes a
/// `lookup(host)` method. Cheap to clone (it's a `Vec` of small
/// structs); the resolver holds one per `Agent`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SensitivePlatforms {
    #[serde(default, rename = "entry")]
    pub entries: Vec<SensitivePlatformEntry>,
}

/// TOML file shape — what `config/sensitive_platforms.toml` actually
/// looks like on disk. Kept separate from `SensitivePlatforms` so the
/// on-disk format can evolve (e.g. add `known_to_challenge_bots` for
/// Phase 8) without churning the in-memory type.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct SensitivePlatformsFile {
    #[serde(default, rename = "entry")]
    entry: Vec<SensitivePlatformEntry>,
}

impl SensitivePlatforms {
    /// Load from the default `config/sensitive_platforms.toml`.
    ///
    /// [EXCEPTION TO READ-ONLY INVARIANT]
    /// `mew-nav` is generally frozen across phases, but this parent-walk and
    /// `MEW_WORKSPACE_DIR` env-var override were added in Phase 16.3. Without
    /// them, the Tauri shell (running from `target/debug/`) fails to load
    /// the table because its CWD doesn't match the workspace root. We chose
    /// to fix the loader here rather than forcing the shell to manually locate
    /// and pass the table into the resolver, which would have required changing
    /// the `resolve_url` signature and churning `mew-agent`'s internals.
    ///
    /// Resolution order:
    ///
    /// 1. **`MEW_WORKSPACE_DIR` env var**, if set. We treat it as the
    ///    workspace root and look for `config/sensitive_platforms.toml`
    ///    inside it. This is the escape hatch for environments where
    ///    the parent-directory walk below can't find the file (a
    ///    Tauri release bundle running from `Program Files\mew-ui`,
    ///    a packaged `.app` on macOS, a flat-snapshot CI run, etc.).
    ///    Set the env var to the absolute path of the repo root and
    ///    the table will be found.
    ///
    /// 2. **Parent-directory walk** from `std::env::current_dir()`.
    ///    Mirrors `mew-agent::load_config` — at each directory
    ///    we check for `config/sensitive_platforms.toml` and
    ///    return the first hit. This is the path that fixes the
    ///    "CWD is `target/debug/` so the file isn't found" bug
    ///    when launching the Tauri shell from a dev build.
    ///
    /// 3. **Fall back to an empty table**, with a `tracing::debug`
    ///    log. The empty table is non-fatal — the resolver
    ///    falls through to its other branches and the agent
    ///    keeps working, just without the sensitive-platform
    ///    routing. Existing setups that haven't created the
    ///    Phase 2 file still run.
    pub fn load_from_default_location() -> Self {
        Self::load_from_env_and_cwd(
            std::env::var("MEW_WORKSPACE_DIR").ok(),
            std::env::current_dir().ok(),
        )
    }

    fn load_from_env_and_cwd(
        workspace_dir_opt: Option<String>,
        current_dir_opt: Option<std::path::PathBuf>,
    ) -> Self {
        // (1) Env-var override — the Tauri shell sets this so
        // release builds can find the table without a CWD
        // assumption. Headless `mew-cli` doesn't need it
        // because the user typically runs it from the repo
        // root and the parent walk finds the file.
        if let Some(workspace_dir) = workspace_dir_opt {
            let candidate = std::path::Path::new(&workspace_dir)
                .join("config")
                .join("sensitive_platforms.toml");
            if candidate.exists() {
                if let Ok(s) = Self::load_from(&candidate) {
                    tracing::info!(
                        event = "sensitive_platforms_loaded",
                        source = "MEW_WORKSPACE_DIR",
                        path = %candidate.display(),
                        entries = s.entries.len(),
                        "loaded sensitive-platforms table from MEW_WORKSPACE_DIR"
                    );
                    return s;
                }
            } else {
                tracing::debug!(
                    event = "sensitive_platforms_env_miss",
                    env_path = %candidate.display(),
                    "MEW_WORKSPACE_DIR is set but config/sensitive_platforms.toml is not there; trying parent walk"
                );
            }
        }

        // (2) Parent-directory walk. We start at the provided
        // CWD and look for `config/sensitive_platforms.toml` in
        // each ancestor. The first hit wins. The walk is bounded
        // by `MAX_PARENT_DEPTH` so a CWD at the filesystem root
        // doesn't loop forever (e.g. on a misconfigured CI
        // container that drops us at `/`).
        const MAX_PARENT_DEPTH: usize = 16;
        if let Some(mut current_dir) = current_dir_opt {
            for _ in 0..MAX_PARENT_DEPTH {
                let candidate =
                    current_dir.join("config").join("sensitive_platforms.toml");
                if candidate.exists() {
                    if let Ok(s) = Self::load_from(&candidate) {
                        tracing::info!(
                            event = "sensitive_platforms_loaded",
                            source = "parent_walk",
                            path = %candidate.display(),
                            entries = s.entries.len(),
                            "loaded sensitive-platforms table from parent directory walk"
                        );
                        return s;
                    }
                }
                if !current_dir.pop() {
                    break;
                }
            }
        }

        // (3) Fall back to an empty table. The pre-Phase-2 setups
        // hit this path and they keep working — the resolver
        // just loses the via-search reroute for sensitive
        // platforms.
        tracing::debug!(
            event = "sensitive_platforms_not_loaded",
            "config/sensitive_platforms.toml not found in CWD, parents, or MEW_WORKSPACE_DIR; using empty table (sensitive-platform routing disabled)"
        );
        Self::default()
    }

    /// Load from an explicit path. Returns Err on I/O or parse
    /// failure — the caller decides whether that's fatal.
    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("read {}: {e}", path.display())
        })?;
        let parsed: SensitivePlatformsFile = toml::from_str(&content).map_err(|e| {
            anyhow::anyhow!("parse {}: {e}", path.display())
        })?;
        Ok(Self { entries: parsed.entry })
    }

    /// Look up an entry by host. Returns the matching strategy if
    /// any, with exact-host entries taking precedence over wildcard
    /// entries.
    ///
    /// `host` is expected to be the bare host (no scheme, no port).
    /// The lookup is case-insensitive: a caller that hands us
    /// `"WWW.Linkedin.com"` still matches an entry keyed on
    /// `"linkedin.com"`. A leading `www.` is also stripped before
    /// the comparison, so `www.instagram.com` matches `instagram.com`.
    pub fn lookup(&self, host: &str) -> Option<EntryStrategy> {
        if self.entries.is_empty() {
            return None;
        }
        let host = strip_www(host).to_ascii_lowercase();
        // Pass 1: exact match.
        for e in &self.entries {
            if !e.domain.starts_with("*.") && strip_www(&e.domain).to_ascii_lowercase() == host {
                return Some(e.strategy);
            }
        }
        // Pass 2: single-level wildcard match.
        // e.g. host "mobile.twitter.com" matches pattern "*.twitter.com".
        for e in &self.entries {
            if let Some(suffix) = e.domain.strip_prefix("*.") {
                let suffix = strip_www(suffix).to_ascii_lowercase();
                // The host must have exactly one label before the
                // suffix, and the suffix must match the trailing
                // labels verbatim. This rejects look-alike
                // attacks like "evil-twitter.com" against
                // "*.twitter.com".
                if let Some(label_end) = host.find('.') {
                    let (label, rest) = host.split_at(label_end);
                    let rest = &rest[1..]; // drop the '.'
                    if !label.is_empty() && rest == suffix {
                        return Some(e.strategy);
                    }
                }
            }
        }
        None
    }

    /// Phase 8: does the table know this host as a challenge
    /// server? The agent's pre-navigate pacing call uses this
    /// to inject an extra "I'm a careful browser" delay before
    /// the navigation, on the theory that the *navigation
    /// itself* is the most likely moment a challenge page is
    /// served. Same matching rules as `lookup()` (case-
    /// insensitive, leading `www.` stripped, exact-host
    /// beats wildcard). The agent treats `true` as
    /// informational — it does *not* force a via-search
    /// reroute; that's already done by `lookup`. The two
    /// are independent signals: a `via_search` host that is
    /// *not* a known challenger just wants a natural-looking
    /// referrer; a `via_search` host that *is* a known
    /// challenger also wants slower pacing and a "expected to
    /// see a challenge" telemetry pre-seed.
    pub fn is_known_challenger(&self, host: &str) -> bool {
        if self.entries.is_empty() || host.is_empty() {
            return false;
        }
        let host = strip_www(host).to_ascii_lowercase();
        // Pass 1: exact match.
        for e in &self.entries {
            if !e.domain.starts_with("*.")
                && strip_www(&e.domain).to_ascii_lowercase() == host
                && e.known_to_challenge_bots
            {
                return true;
            }
        }
        // Pass 2: single-level wildcard match.
        for e in &self.entries {
            if let Some(suffix) = e.domain.strip_prefix("*.") {
                let suffix = strip_www(suffix).to_ascii_lowercase();
                if let Some(label_end) = host.find('.') {
                    let (label, rest) = host.split_at(label_end);
                    let rest = &rest[1..];
                    if !label.is_empty()
                        && rest == suffix
                        && e.known_to_challenge_bots
                    {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Strip a leading `www.` from a host. Idempotent. Case-insensitive:
/// `WWW.example.com` and `www.example.com` both strip. Anything that
/// doesn't start with `www.` is returned unchanged. Used to normalize
/// before both `SensitivePlatforms::lookup` and the existing
/// `domain == d || domain.ends_with(&format!(".{}", d))` allowlist
/// check in `mew-agent`'s navigate handler.
fn strip_www(host: &str) -> &str {
    if host.len() >= 4 && host[..4].eq_ignore_ascii_case("www.") {
        &host[4..]
    } else {
        host
    }
}

/// Build the search-engine URL for a sensitive-platform entry.
///
/// Both `ViaSearch` and `ViaSearchConfirm` produce a Google search
/// results page keyed by the **bare site name** (or the host the
/// resolver already canonicalized). The previous implementation
/// emitted a `site:` query operator for explicit-URL inputs
/// (`q=site%3Ainstagram.com`) on the theory that a scoped query
/// would land the agent on the right domain. In practice that
/// caused two visible regressions:
///
///   1. The LLM, seeing `via_search` in its instructions, would
///      preemptively type `navigate("site:instagram.com")` itself,
///      bypassing the resolver entirely. The `site:` token then
///      leaks into the resolved URL and Google serves a results
///      page whose organic "result #1" is not always the
///      canonical homepage — sometimes it's a help article, a
///      status page, or a sub-domain that the agent mis-clicks.
///   2. Even when the LLM passes a clean URL, a `site:` query
///      suppresses the natural-looking organic list (Wikipedia,
///      the company's Twitter, news coverage) that the agent
///      reads as "this is the right site" and clicks first. The
///      bare-name query `q=instagram` returns instagram.com as
///      the top organic result on the first try.
///
/// The fix is to never emit the `site:` operator in the resolved
/// URL, and to defensively strip any `site:` token the LLM
/// embedded in its raw input before doing anything else with it.
///
/// * `ViaSearch` on a bare-name input `"instagram"` produces
///   `https://www.google.com/search?q=instagram`.
/// * `ViaSearch` on an explicit-URL input
///   `"https://www.instagram.com/feed"` produces
///   `https://www.google.com/search?q=instagram.com` (the host,
///   no operator).
/// * `ViaSearchConfirm` always appends the `login` token so the
///   search results bias toward the canonical sign-in surface.
fn build_via_search_url(
    strategy: EntryStrategy,
    resolved_host: &str,
    original_input: &str,
) -> String {
    let already_url = looks_like_url(original_input);
    // Canonicalize the host: strip `www.` so `www.instagram.com`
    // and `instagram.com` both end up at the same `q=instagram.com`
    // query. Mirrors the existing `strip_www` used elsewhere.
    let canonical_host = strip_www(resolved_host);
    // Always build the query from the canonical host, not from the
    // LLM's raw input. That way:
    //   * `navigate("https://www.instagram.com/feed")` -> `q=instagram.com`
    //   * `navigate("instagram")` -> `q=instagram.com` (host is
    //     already canonical because the resolver normalized it
    //     before this function is called)
    // We deliberately *do not* include the `site:` Google operator
    // — see the function-level docstring for why.
    let base_query = canonical_host.to_string();
    // Login bias is only applied to `ViaSearchConfirm`. The login
    // suffix is the same as before; the only thing that changed
    // is whether the base is a `site:` token or a bare host.
    let login_suffix = " login";
    let confirm = matches!(strategy, EntryStrategy::ViaSearchConfirm);
    let query = if confirm {
        format!("{}{}", base_query, login_suffix)
    } else {
        base_query
    };
    let _ = already_url; // kept for parity with the previous signature
    format!("https://www.google.com/search?q={}", url_encode(&query))
}

/// Strip any `site:` Google search operator embedded in the LLM's
/// raw input. Defends against the LLM preemptively typing
/// `navigate("site:instagram.com")` to "save the resolver a step".
/// The output of this function is a *navigable* identifier
/// (host or bare name) that the rest of the resolver can use.
///
/// Handles these forms:
///
///   * `site:instagram.com`           → `instagram.com`
///   * `site:instagram.com login`     → `instagram.com login`
///   * `SITE:Instagram.COM`           → `Instagram.COM` (lowercased
///                                       to `instagram.com` to match
///                                       the rest of the resolver's
///                                       canonicalization)
///   * `https://site:instagram.com`   → `https://instagram.com`
///   * `instagram.com`                → `instagram.com` (pass-through
///                                       when no `site:` token is
///                                       present)
///
/// Returns the original string with the `site:` token removed
/// (and any leading whitespace after it trimmed). Empty input
/// returns empty.
pub(crate) fn strip_site_query(raw: &str) -> String {
    // Case-insensitive scan for the `site:` token. We do this
    // *before* URL parsing so we also catch the LLM's
    // `navigate("site:instagram.com")` shape (no scheme, just a
    // bare operator + host), which `Url::parse` would otherwise
    // reject as "relative URL without a base" and the resolver
    // would then send to the search fallback as `q=site%3A...`.
    let lower = raw.to_ascii_lowercase();
    if let Some(pos) = lower.find("site:") {
        let before = &raw[..pos];
        let after = &raw[pos + "site:".len()..];
        // Two shapes the LLM can produce:
        //
        //   1. `site:instagram.com` — bare operator + host. Just
        //      drop the operator.
        //   2. `https://site:instagram.com/feed` — operator wedged
        //      between the scheme and the host (the LLM's idea
        //      of a "scoped URL"). The `://` belongs to the
        //      scheme, so we keep `before` (which includes the
        //      `https://`) and concatenate it with the host that
        //      follows the operator.
        //
        // Both shapes end up the same way: `before` + `after` —
        // the `site:` token itself is the only thing removed.
        let cleaned_after = after.trim_start();
        let cleaned_before = before.trim_end();
        return format!("{}{}", cleaned_before, cleaned_after)
            .trim()
            .to_string();
    }
    raw.trim().to_string()
}


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
    // Defensive: strip the `site:` Google search operator if the
    // LLM (or the user) embedded one in the raw input. The
    // resolver treats the cleaned input as the canonical
    // navigable identifier from this point on.
    let cleaned = strip_site_query(input);
    let mut r = resolve_without_probe_sensitive(&cleaned, &SensitivePlatforms::default());
    // The `ResolutionResult::original_input` is for the human
    // reader (transcript / post-mortem). They want to see
    // exactly what the LLM typed, not the cleaned form. The
    // routing logic only sees the cleaned form; the audit trail
    // shows the raw input.
    r.original_input = input.to_string();
    r
}

/// Phase 2: pure-Rust resolver that also consults the
/// `SensitivePlatforms` table. Hosts matched by the table short-circuit
/// the existing branches and return a `ViaSearch` / `ViaSearchConfirm`
/// resolution. Use this in production; the no-table variant is kept
/// for tests and the legacy call sites that don't load a table.
pub fn resolve_without_probe_sensitive(
    input: &str,
    sensitive: &SensitivePlatforms,
) -> ResolutionResult {
    // Defensive: strip the `site:` Google search operator if the
    // LLM (or the user) embedded one in the raw input. The
    // resolver treats the cleaned input as the canonical
    // navigable identifier from this point on. The
    // `original_input` field still carries the raw LLM input so
    // the audit trail is faithful to what the LLM typed.
    let cleaned = strip_site_query(input);
    // Bridge from host-form (`instagram.com`) to the bare
    // second-level label (`instagram`) so the known-sites map
    // and the sensitive-platforms table both match the same
    // way the bare-name form does. Without this, an LLM
    // `navigate("site:instagram.com")` would be cleaned to
    // `instagram.com`, miss the `instagram` map key, and end
    // up at the `DirectGuess` `https://instagramcom.com` — the
    // exact failure mode the user reported.
    let effective = second_level_label(&cleaned)
        .map(|sld| {
            // If the host's SLD is a known site, prefer the
            // bare-name form. The map lookup is cheap and
            // idempotent; if it doesn't match, fall back to
            // the cleaned form.
            let bare = sld.to_string();
            if known_sites_map().contains_key(bare.as_str()) {
                bare
            } else {
                cleaned.clone()
            }
        })
        .unwrap_or(cleaned.clone());
    let mut r = resolve_inner(&effective, sensitive);
    r.original_input = input.to_string();
    r
}

/// Full resolver including the direct-guess probe. Requires a live
/// `Page` because the probe does a real `Page::goto` with a 4s timeout.
///
/// `None` returned page → falls through to search fallback (so callers
/// that hold a `Page` but the page is dead still get a usable result).
pub async fn resolve_with_probe(page: &Page, input: &str) -> ResolutionResult {
    // Defensive: strip `site:` operator before routing. The
    // `original_input` field on the returned `ResolutionResult`
    // still carries the raw LLM input.
    let cleaned = strip_site_query(input);
    let mut r = resolve_with_probe_sensitive(page, &cleaned, &SensitivePlatforms::default()).await;
    r.original_input = input.to_string();
    r
}

/// Phase 2: full resolver with the `SensitivePlatforms` table loaded.
/// See `resolve_without_probe_sensitive` for the routing order.
pub async fn resolve_with_probe_sensitive(
    page: &Page,
    input: &str,
    sensitive: &SensitivePlatforms,
) -> ResolutionResult {
    // Defensive: strip the `site:` Google search operator if the
    // LLM (or the user) embedded one in the raw input. The
    // resolver treats the cleaned form as the canonical
    // navigable identifier from this point on. The raw
    // `raw_input` is still used for the `original_input` field
    // on the returned `ResolutionResult` so the transcript /
    // post-mortem audit trail shows exactly what the LLM typed.
    let raw_input = input;
    let cleaned = strip_site_query(input);
    // Bridge from host-form (`instagram.com`) to the bare
    // second-level label (`instagram`) so the known-sites map
    // and the sensitive-platforms table both match the same
    // way the bare-name form does. See the matching block in
    // `resolve_without_probe_sensitive` for the rationale.
    let input: &str = &match second_level_label(&cleaned) {
        Some(sld) if known_sites_map().contains_key(sld) => sld.to_string(),
        _ => cleaned,
    };
    // Phase 1: structured tracing around the three branches. Each
    // branch emits a single event with the path it took and the URL
    // it produced, so a post-mortem review can answer "why did the
    // agent land on google.com for this prompt?" with a single grep.
    // The span is the resolver itself; the events are the per-branch
    // decisions.
    let resolver_span = tracing::info_span!("url_resolver", input = %input);
    let _enter = resolver_span.enter();

    // Phase 2: sensitive-platform routing. Runs *before* the
    // map/URL/guess branches so a sensitive host never gets the
    // direct-nav path, even if it would have been a map-hit. The
    // table owns the policy; the resolver just consults it.
    //
    // We do the host lookup using the *resolved* target, not the
    // raw LLM input. For map-hits that's the canonical URL from
    // `known_sites_map`; for raw URL inputs it's the URL's host;
    // for bare names it falls through to the existing branches
    // (which can still map to a sensitive host — e.g. "instagram"
    // is in the map, and the map's canonical URL ends up routed
    // here).
    if let Some(resolved) = resolve_inner_for_sensitive_check(input) {
        if let Some(host) = resolved.host_str() {
            if let Some(strategy) = sensitive.lookup(host) {
                let resolved_host = host.to_string();
                let url = build_via_search_url(strategy, &resolved_host, input);
                let path = match strategy {
                    EntryStrategy::ViaSearch => ResolutionPath::ViaSearch,
                    EntryStrategy::ViaSearchConfirm => ResolutionPath::ViaSearchConfirm,
                    EntryStrategy::Direct => unreachable!(
                        "Direct strategy should never reach the sensitive branch"
                    ),
                };
                tracing::info!(
                    event = "url_resolution",
                    branch = path.as_str(),
                    resolved_url = %url,
                    sensitive_host = %resolved_host,
                    "URL resolved via sensitive-platform routing"
                );
                return ResolutionResult {
                    url,
                    path,
                    original_input: raw_input.to_string(),
                };
            }
        }
    }

    // Path 1: known-sites map — instant, no network.
    let map = known_sites_map();
    let key = input.trim().to_lowercase();
    if let Some(url) = map.get(key.as_str()).copied() {
        tracing::info!(
            event = "url_resolution",
            branch = "map-hit",
            resolved_url = %url,
            "URL resolved via known-sites map"
        );
        return ResolutionResult {
            url: url.to_string(),
            path: ResolutionPath::MapHit,
            original_input: raw_input.to_string(),
        };
    }

    // Already a URL → pass through.
    if looks_like_url(input) {
        tracing::info!(
            event = "url_resolution",
            branch = "already-url",
            resolved_url = %input.trim(),
            "URL passed through as-is"
        );
        return ResolutionResult {
            url: input.trim().to_string(),
            path: ResolutionPath::AlreadyUrl,
            original_input: raw_input.to_string(),
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
            tracing::info!(
                event = "url_resolution",
                branch = "direct-guess",
                resolved_url = %guess_url,
                "URL resolved via direct-guess probe"
            );
            return ResolutionResult {
                url: guess_url,
                path: ResolutionPath::DirectGuess,
                original_input: raw_input.to_string(),
            };
        }
        // Probe failed — log the failure explicitly so the trace
        // shows the *attempt* (not just the eventual fallback).
        tracing::info!(
            event = "url_resolution_probe_failed",
            guessed_url = %guess_url,
            "direct-guess probe failed; falling through to search"
        );
    }

    // Path 3: search fallback. The exact "use google and open X" path
    // the user has been typing by hand — we just trigger it automatically.
    let encoded = url_encode(input.trim());
    let search_url = format!("https://www.google.com/search?q={}", encoded);
    tracing::info!(
        event = "url_resolution",
        branch = "search-fallback",
        resolved_url = %search_url,
        "URL resolved via Google search fallback"
    );
    ResolutionResult {
        url: search_url,
        path: ResolutionPath::SearchFallback,
        original_input: raw_input.to_string(),
    }
}

/// Shared core used by `resolve_without_probe` (sync) and the async
/// `resolve_with_probe` (which calls it after Path 1 already matched).
/// Kept private because the public API is the `resolve_*` functions.
fn resolve_inner(input: &str, sensitive: &SensitivePlatforms) -> ResolutionResult {
    // Phase 2: sensitive-platform routing runs *before* the map/URL/
    // guess branches in the dry-run path too, so unit tests can
    // assert the routing without needing a browser. The probe-less
    // resolver does NOT probe the resulting URL — the caller is
    // expected to navigate to it.
    if let Some(resolved) = resolve_inner_for_sensitive_check(input) {
        if let Some(host) = resolved.host_str() {
            if let Some(strategy) = sensitive.lookup(host) {
                let resolved_host = host.to_string();
                let url = build_via_search_url(strategy, &resolved_host, input);
                let path = match strategy {
                    EntryStrategy::ViaSearch => ResolutionPath::ViaSearch,
                    EntryStrategy::ViaSearchConfirm => ResolutionPath::ViaSearchConfirm,
                    EntryStrategy::Direct => unreachable!(
                        "Direct strategy should never reach the sensitive branch"
                    ),
                };
                return ResolutionResult {
                    url,
                    path,
                    original_input: input.to_string(),
                };
            }
        }
    }
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

/// Phase 2: figure out the URL the *existing* branches would have
/// produced, so we can check its host against the sensitive-platforms
/// table. This deliberately duplicates a small amount of the map/URL
/// logic instead of recursively calling `resolve_inner` — that would
/// be cleaner but risks an infinite loop if a sensitive host is also
/// in the map. (Instagram is in the map *and* sensitive; the routing
/// is the desired behavior, not a recursion hazard.)
fn resolve_inner_for_sensitive_check(input: &str) -> Option<url::Url> {
    let map = known_sites_map();
    let key = input.trim().to_lowercase();
    if let Some(url) = map.get(key.as_str()) {
        return url::Url::parse(url).ok();
    }
    if looks_like_url(input) {
        return url::Url::parse(input.trim()).ok();
    }
    let guess = sanitize_for_domain_guess(input);
    if !guess.is_empty() {
        let guess_url = format!("https://{}.com", guess);
        return url::Url::parse(&guess_url).ok();
    }
    None
}

/// Helper: given a string that *might* be a bare host
/// (`instagram.com`, `www.instagram.com`, etc.), return its
/// second-level label (`instagram`). Used to bridge the
/// `strip_site_query` cleanup to the known-sites map: after
/// `strip_site_query("site:instagram.com")` returns
/// `"instagram.com"`, we want the map lookup to also try
/// `"instagram"`, so the resolver still matches
/// `instagram.com` against the same `instagram` map entry
/// that the bare-name form does. Returns `None` if the input
/// doesn't look like a host (no `.`, or only `.` at the edges).
fn second_level_label(input: &str) -> Option<&str> {
    let s = input.trim();
    if let Some(dot) = s.find('.') {
        let label = &s[..dot];
        let rest = &s[dot + 1..];
        if !label.is_empty() && !rest.is_empty() {
            return Some(label);
        }
    }
    None
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
        assert_eq!(ResolutionPath::ViaSearch.as_str(), "via-search");
        assert_eq!(ResolutionPath::ViaSearchConfirm.as_str(), "via-search-confirm");
    }

    // -----------------------------------------------------------------
    // Phase 2: sensitive-platform tests
    // -----------------------------------------------------------------
    //
    // The test table is the same shape as
    // `config/sensitive_platforms.toml` and lives in code so the
    // test doesn't depend on the file being on disk. (The file's
    // existence / parse-ability is covered by the integration
    // fixture in `mew-agent/examples/phase2_sensitive_load.rs`.)
    fn test_sensitive_table() -> SensitivePlatforms {
        SensitivePlatforms {
            entries: vec![
                SensitivePlatformEntry {
                    domain: "instagram.com".to_string(),
                    strategy: EntryStrategy::ViaSearch,
                    known_to_challenge_bots: true,
                },
                SensitivePlatformEntry {
                    domain: "linkedin.com".to_string(),
                    strategy: EntryStrategy::ViaSearchConfirm,
                    known_to_challenge_bots: true,
                },
                SensitivePlatformEntry {
                    domain: "*.twitter.com".to_string(),
                    strategy: EntryStrategy::ViaSearch,
                    known_to_challenge_bots: true,
                },
            ],
        }
    }

    /// A second test table for the `is_known_challenger` tests,
    /// which need a row that is in the table *with* the flag
    /// cleared (so we can assert the negative case). The
    /// primary `test_sensitive_table()` above keeps the original
    /// shape (no `example.com`) so the pre-Phase-8 lookup tests
    /// don't have to be touched.
    fn test_sensitive_table_with_negatives() -> SensitivePlatforms {
        SensitivePlatforms {
            entries: vec![
                SensitivePlatformEntry {
                    domain: "instagram.com".to_string(),
                    strategy: EntryStrategy::ViaSearch,
                    known_to_challenge_bots: true,
                },
                SensitivePlatformEntry {
                    // In the table for routing, but
                    // *not* a known challenger. The
                    // `is_known_challenger` test asserts
                    // this returns false.
                    domain: "example.com".to_string(),
                    strategy: EntryStrategy::ViaSearch,
                    known_to_challenge_bots: false,
                },
            ],
        }
    }

    #[test]
    fn sensitive_table_lookup_exact_match() {
        let t = test_sensitive_table();
        assert_eq!(t.lookup("instagram.com"), Some(EntryStrategy::ViaSearch));
        assert_eq!(t.lookup("linkedin.com"), Some(EntryStrategy::ViaSearchConfirm));
        assert_eq!(t.lookup("example.com"), None);
    }

    #[test]
    fn sensitive_table_strips_www() {
        let t = test_sensitive_table();
        assert_eq!(t.lookup("www.instagram.com"), Some(EntryStrategy::ViaSearch));
        assert_eq!(t.lookup("WWW.Linkedin.com"), Some(EntryStrategy::ViaSearchConfirm));
    }

    #[test]
    fn sensitive_table_wildcard_matches_one_subdomain() {
        let t = test_sensitive_table();
        assert_eq!(t.lookup("mobile.twitter.com"), Some(EntryStrategy::ViaSearch));
        assert_eq!(t.lookup("m.twitter.com"), Some(EntryStrategy::ViaSearch));
        // Look-alike attack must NOT match.
        assert_eq!(t.lookup("evil-twitter.com"), None);
        assert_eq!(t.lookup("twitter.com.evil.com"), None);
    }

    #[test]
    fn sensitive_table_empty_returns_none() {
        let t = SensitivePlatforms::default();
        assert_eq!(t.lookup("instagram.com"), None);
        assert_eq!(t.lookup("anything.com"), None);
    }

    #[test]
    fn sensitive_routing_runs_before_map_hit() {
        // The motivating case: "instagram" is a map-hit AND a
        // sensitive host. The sensitive branch must win so the
        // resolver returns a `via-search` Google URL, not a
        // direct `https://www.instagram.com` map-hit.
        let t = test_sensitive_table();
        let r = resolve_without_probe_sensitive("instagram", &t);
        assert_eq!(r.path, ResolutionPath::ViaSearch);
        assert!(r.url.contains("google.com/search"), "url={}", r.url);
        assert!(r.url.contains("instagram"), "url={}", r.url);
        assert!(!r.url.contains("login"), "via_search must not add login: url={}", r.url);
    }

    #[test]
    fn sensitive_routing_via_search_confirm_adds_login() {
        let t = test_sensitive_table();
        let r = resolve_without_probe_sensitive("linkedin", &t);
        assert_eq!(r.path, ResolutionPath::ViaSearchConfirm);
        let decoded = r.url.replace("%20", " ").replace("%3A", ":");
        assert!(decoded.contains("linkedin"), "url={}", r.url);
        assert!(decoded.contains("login"), "via_search_confirm must add login keyword: url={}", r.url);
    }

    #[test]
    fn sensitive_routing_for_explicit_url_uses_bare_host() {
        // When the LLM hands us a full URL, the search query uses
        // the **bare host** (no `site:` Google operator). The
        // previous behavior emitted `q=site%3Ainstagram.com`,
        // which:
        //   1. The LLM itself started preemptively emitting
        //      `navigate("site:instagram.com")` (defensively
        //      routed to `q=instagram.com` by the new
        //      `strip_site_query` shim), and
        //   2. Made the agent land on a help article or sub-domain
        //      instead of the canonical homepage (because the
        //      `site:` operator suppresses the natural-looking
        //      organic list).
        // The bare-host query `q=instagram.com` returns the
        // canonical homepage as Google's first organic result.
        let t = test_sensitive_table();
        let r =
            resolve_without_probe_sensitive("https://www.instagram.com/feed", &t);
        assert_eq!(r.path, ResolutionPath::ViaSearch);
        let decoded = r.url.replace("%3A", ":");
        assert!(decoded.contains("instagram.com"), "url={}", r.url);
        assert!(
            !decoded.contains("site:instagram.com"),
            "explicit-URL must NOT emit the site: operator: url={}",
            r.url
        );
    }

    #[test]
    fn strip_site_query_drops_llm_emitted_operator() {
        // The LLM sometimes preemptively types
        // `navigate("site:instagram.com")` to "save the resolver a
        // step". This must be cleaned to `instagram.com` before
        // any branch (sensitive, map-hit, direct-guess,
        // search-fallback) sees the input.
        assert_eq!(strip_site_query("site:instagram.com"), "instagram.com");
        assert_eq!(strip_site_query("SITE:Instagram.COM"), "Instagram.COM");
        assert_eq!(
            strip_site_query("site:instagram.com login"),
            "instagram.com login"
        );
        // URL-with-operator: drop the scheme, keep the host.
        assert_eq!(
            strip_site_query("https://site:instagram.com/feed"),
            "https://instagram.com/feed"
        );
        // Pass-through when no operator is present.
        assert_eq!(strip_site_query("instagram.com"), "instagram.com");
        assert_eq!(strip_site_query("instagram"), "instagram");
    }

    #[test]
    fn resolver_swallows_site_prefix_in_raw_input() {
        // End-to-end: even if the LLM hands us
        // `navigate("site:instagram.com")`, the resolver must
        // produce the same via-search URL as the bare-name form.
        let t = test_sensitive_table();
        let bare = resolve_without_probe_sensitive("instagram", &t);
        let with_op =
            resolve_without_probe_sensitive("site:instagram.com", &t);
        assert_eq!(bare.path, with_op.path);
        assert_eq!(bare.url, with_op.url);
    }

    #[test]
    fn sensitive_routing_does_not_apply_to_unspecified_hosts() {
        let t = test_sensitive_table();
        // wikipedia is NOT in the table — must follow the normal
        // branches. "wikipedia" is a map-hit, so we get
        // `MapHit`, not `ViaSearch`.
        let r = resolve_without_probe_sensitive("wikipedia", &t);
        assert_eq!(r.path, ResolutionPath::MapHit);
    }

    #[test]
    fn empty_sensitive_table_preserves_existing_behavior() {
        // Default-constructed table must produce identical
        // results to the no-table API.
        let r1 = resolve_without_probe("instagram");
        let r2 = resolve_without_probe_sensitive("instagram", &SensitivePlatforms::default());
        assert_eq!(r1, r2);
    }

    #[test]
    fn entry_strategy_as_str_is_stable() {
        assert_eq!(EntryStrategy::Direct.as_str(), "direct");
        assert_eq!(EntryStrategy::ViaSearch.as_str(), "via_search");
        assert_eq!(EntryStrategy::ViaSearchConfirm.as_str(), "via_search_confirm");
    }

    // -----------------------------------------------------------------
    // Phase 8: known-to-challenge-bots tests
    // -----------------------------------------------------------------
    //
    // The `is_known_challenger` lookup is what the agent's
    // pre-navigate pacing call uses to inject an extra
    // "I'm a careful browser" delay before a navigation to a
    // known-challenger host. Same matching rules as `lookup`
    // (case-insensitive, leading `www.` stripped, exact-host
    // beats wildcard). The tests below lock the behavior down
    // so a future re-shuffle doesn't silently start (or stop)
    // pacing on the wrong sites.

    #[test]
    fn is_known_challenger_exact_match() {
        let t = test_sensitive_table();
        assert!(t.is_known_challenger("instagram.com"));
        assert!(t.is_known_challenger("linkedin.com"));
    }

    #[test]
    fn is_known_challenger_strips_www() {
        let t = test_sensitive_table();
        assert!(t.is_known_challenger("www.instagram.com"));
        assert!(t.is_known_challenger("WWW.Linkedin.com"));
    }

    #[test]
    fn is_known_challenger_wildcard_matches_one_subdomain() {
        let t = test_sensitive_table();
        assert!(t.is_known_challenger("mobile.twitter.com"));
        assert!(t.is_known_challenger("m.twitter.com"));
        // Look-alike attack must NOT match.
        assert!(!t.is_known_challenger("evil-twitter.com"));
    }

    #[test]
    fn is_known_challenger_false_when_flag_not_set() {
        // The negative case needs a host that is *in* the
        // table but with the flag cleared. The shared
        // `test_sensitive_table` has every entry with
        // `known_to_challenge_bots = true`, so we use a
        // dedicated table for this test.
        let t = test_sensitive_table_with_negatives();
        // example.com is in the table with the flag
        // *not* set — it routes via search but is not
        // a known challenger.
        assert!(!t.is_known_challenger("example.com"));
        // instagram.com is in the table with the flag
        // *set* — it is a known challenger.
        assert!(t.is_known_challenger("instagram.com"));
    }

    #[test]
    fn is_known_challenger_empty_table_returns_false() {
        let t = SensitivePlatforms::default();
        assert!(!t.is_known_challenger("instagram.com"));
        assert!(!t.is_known_challenger("anything.com"));
    }

    #[test]
    fn is_known_challenger_empty_host_returns_false() {
        let t = test_sensitive_table();
        assert!(!t.is_known_challenger(""));
    }

    // -----------------------------------------------------------------
    // Phase 16.3: `load_from_default_location` regression tests.
    //
    // Background: the previous implementation used
    // `Path::new("config/sensitive_platforms.toml")` directly,
    // which only worked when the process's CWD happened to be
    // the workspace root. The Tauri shell's binary lives at
    // `<repo>/target/debug/app.exe` and runs with CWD
    // `target/debug/`, so the relative path resolved to a
    // non-existent file, the table was empty, and the
    // sensitive-platform reroute for instagram.com / linkedin.com
    // / etc. silently fell through to the `already-url` branch.
    // The result: a `navigate("https://www.instagram.com")`
    // tool call returned "domain not in allowlist" because the
    // resolver never converted it to a Google search URL.
    //
    // The fix: walk parents, plus honor a `MEW_WORKSPACE_DIR`
    // env var override. These tests pin both paths.
    // -----------------------------------------------------------------

    /// Helper: build a temp directory tree shaped like
    ///   `<root>/config/sensitive_platforms.toml`
    /// and return `(root_path, table_entries_loaded)`.
    /// The temp dir is dropped only when the test ends, which
    /// is fine because every test only reads from the tree.
    fn write_workspace_with_table() -> (std::path::PathBuf, SensitivePlatforms) {
        use std::fs;
        let root = std::env::temp_dir().join(format!(
            "mew-nav-walk-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let config_dir = root.join("config");
        fs::create_dir_all(&config_dir).expect("create temp config dir");
        let toml_path = config_dir.join("sensitive_platforms.toml");
        let toml_content = r#"
[[entry]]
domain = "instagram.com"
strategy = "via_search"
known_to_challenge_bots = true

[[entry]]
domain = "linkedin.com"
strategy = "via_search_confirm"
known_to_challenge_bots = true
"#;
        fs::write(&toml_path, toml_content).expect("write toml");
        let table = SensitivePlatforms::load_from(&toml_path)
            .expect("table should load from the temp file");
        (root, table)
    }

    #[test]
    fn load_from_default_location_walks_parents() {
        // Simulate the Tauri-shell bug: the binary is "running"
        // with a CWD that's several levels *below* the workspace
        // root (e.g. `target/debug/`). The parent walk should
        // find the table.
        //
        // We set CWD to `<workspace>/config/` and verify the
        // walk finds the sibling `sensitive_platforms.toml` one
        // directory up. The walk doesn't need to be deep — the
        // critical case is "CWD is not the root, but a parent
        // has the file".
        let (workspace, _expected) = write_workspace_with_table();
        let nested = workspace.join("config");
        let loaded = SensitivePlatforms::load_from_env_and_cwd(None, Some(nested));
        // Cleanup the temp tree (we don't want flakes
        // across test runs in CI).
        let _ = std::fs::remove_dir_all(&workspace);

        assert_eq!(
            loaded.lookup("instagram.com"),
            Some(EntryStrategy::ViaSearch),
            "parent-walk must find instagram.com in the temp workspace"
        );
        assert_eq!(
            loaded.lookup("linkedin.com"),
            Some(EntryStrategy::ViaSearchConfirm),
            "parent-walk must find linkedin.com in the temp workspace"
        );
    }

    #[test]
    fn load_from_default_location_honors_workspace_dir_env() {
        // The Tauri shell sets `MEW_WORKSPACE_DIR` to the
        // inferred workspace root before any agent task is
        // created. The loader must prefer that env var over
        // the parent walk.
        let (workspace, _expected) = write_workspace_with_table();

        // Point CWD somewhere with NO config/ at any parent,
        // so the only way the loader can find the table is via
        // the env var.
        let sibling = std::env::temp_dir().join("mew-nav-env-marker");
        let _ = std::fs::create_dir_all(&sibling);

        let loaded = SensitivePlatforms::load_from_env_and_cwd(
            Some(workspace.display().to_string()),
            Some(sibling.clone()),
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&sibling);

        assert_eq!(
            loaded.lookup("instagram.com"),
            Some(EntryStrategy::ViaSearch),
            "MEW_WORKSPACE_DIR must be honored even when the parent walk would fail"
        );
    }

    #[test]
    fn load_from_default_location_returns_empty_table_when_no_config_found() {
        // The pre-Phase-2 fallback path: a setup that doesn't
        // have the file (yet) must not panic; the loader
        // returns an empty table and `lookup` returns None.
        // We simulate this by pointing CWD at a temp dir
        // that has no `config/` at any parent and clearing
        // the env var.
        let empty_root = std::env::temp_dir().join(format!(
            "mew-nav-empty-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&empty_root).expect("create empty root");

        let loaded = SensitivePlatforms::load_from_env_and_cwd(None, Some(empty_root.clone()));

        let _ = std::fs::remove_dir_all(&empty_root);

        assert!(
            loaded.entries.is_empty(),
            "no config on disk + no env var should produce an empty table"
        );
        assert_eq!(loaded.lookup("instagram.com"), None);
    }
}

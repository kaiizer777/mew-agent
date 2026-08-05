//! Phase 6 — Failure mode 4: Rate-limit cliff detection.
//!
//! Background: the agent's pacing layer (`mew-agent::pacing`) handles
//! the "click 47 times in 2 seconds" anti-bot case on the *client*
//! side. That doesn't help when the *server* pushes back. A 429,
//! 503, or Cloudflare interstitial replaces the real page; the LLM,
//! looking at an empty/short tree, has no idea why. Without
//! detection the agent will keep clicking, each click refiring
//! against the same interstitial, the backoff never escalating.
//!
//! The fix: detect the rate-limit signal in the rendered tree (the
//! page content the LLM actually sees) and surface it as a typed
//! finding. The agent loop uses the `RateLimitKind` to choose the
//! backoff curve — a 429 with `Retry-After: 30` should wait
//! exactly 30 seconds, while a Cloudflare interstitial without a
//! server-provided hint gets a default exponential curve.
//!
//! Detection is heuristic on the *body text* of the page. We
//! recognize three common shapes:
//!   * "429 Too Many Requests" / "Rate limit exceeded" — explicit
//!     HTTP error pages (GitHub, Twitter, Reddit).
//!   * Cloudflare interstitial — "Checking your browser before
//!     accessing" / "DDoS protection by Cloudflare".
//!   * Generic "Access denied" / "Slow down" / "Bot detection" —
//!     a softer signal for sites that don't use Cloudflare but do
//!     rate-limit.
//!
//! Pure-Rust, no I/O. Tests cover all three shapes plus a clean
//! page negative case.

use mew_perception::TreeNode;

/// What kind of rate-limit signal the detector saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateLimitKind {
    /// HTTP 429 page (or an explicit "Too Many Requests" text).
    Http429,
    /// Cloudflare interstitial (browser challenge).
    Cloudflare,
    /// Generic "access denied" / "slow down" / "bot detection"
    /// shape — usually a custom site-level limiter.
    Generic,
}

impl RateLimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RateLimitKind::Http429 => "http_429",
            RateLimitKind::Cloudflare => "cloudflare",
            RateLimitKind::Generic => "generic",
        }
    }

    /// The default backoff in seconds for this kind, when no
    /// server-provided hint is present. Used by the agent loop to
    /// compute a sleep duration.
    pub fn default_backoff_secs(self) -> u64 {
        match self {
            // 429 is a server request: respect the hint when
            // present, otherwise default to 30s (a typical
            // sliding-window for a public API).
            RateLimitKind::Http429 => 30,
            // Cloudflare interstitials auto-resolve in 5-10s
            // when the JS challenge passes. 15s gives the
            // challenge time to complete and the page to load.
            RateLimitKind::Cloudflare => 15,
            // Generic: the agent has no hint. Default to 10s —
            // long enough to clear most rate-limit windows, short
            // enough to not feel like a hang.
            RateLimitKind::Generic => 10,
        }
    }
}

/// The detector's verdict. `label` is a short human-readable
/// string for the user-facing chat reply. The agent loop uses
/// `kind` to pick the backoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitSignal {
    pub label: String,
    pub kind: RateLimitKind,
}

/// Scan the tree for the three rate-limit signals. The detector
/// walks the tree depth-first and concatenates the names of all
/// text-bearing nodes into a single lowercase blob, then runs the
/// pattern match. Concatenation is bounded (we stop after N
/// characters) to avoid pathological cost on huge trees.
///
/// The first match wins. We classify in priority order: explicit
/// HTTP 429 > Cloudflare > generic. A 429 with "Cloudflare" in
/// the same text is reported as 429 (the more specific signal).
pub fn detect(tree: &TreeNode) -> Option<RateLimitSignal> {
    const MAX_TEXT_BYTES: usize = 8192;
    let mut buf = String::new();
    collect_text(tree, &mut buf, MAX_TEXT_BYTES);
    let lower = buf.to_lowercase();

    if lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("rate limit exceeded")
        || lower.contains("rate-limit exceeded")
    {
        return Some(RateLimitSignal {
            label: "HTTP 429 / rate-limit page detected".to_string(),
            kind: RateLimitKind::Http429,
        });
    }

    if lower.contains("checking your browser")
        || lower.contains("ddos protection")
        || lower.contains("cloudflare")
        || lower.contains("cf-ray")
        || lower.contains("verify you are human")
    {
        return Some(RateLimitSignal {
            label: "Cloudflare / browser-challenge interstitial detected".to_string(),
            kind: RateLimitKind::Cloudflare,
        });
    }

    if lower.contains("access denied")
        || lower.contains("slow down")
        || lower.contains("bot detection")
        || lower.contains("are you a bot")
        || lower.contains("unusual traffic")
    {
        return Some(RateLimitSignal {
            label: "Generic rate-limit / bot-detection page detected".to_string(),
            kind: RateLimitKind::Generic,
        });
    }

    None
}

fn collect_text(node: &TreeNode, out: &mut String, cap: usize) {
    if out.len() >= cap {
        return;
    }
    if !node.name.is_empty() {
        out.push(' ');
        out.push_str(&node.name);
    }
    if !node.value.is_empty() {
        out.push(' ');
        out.push_str(&node.value);
    }
    for child in &node.children {
        collect_text(child, out, cap);
        if out.len() >= cap {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mew_perception::NodeCategory;

    fn node(role: &str, name: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            id: format!("n_{}_{}", role, name),
            role: role.to_string(),
            name: name.to_string(),
            value: String::new(),
            category: NodeCategory::Structural,
            ref_id: None,
            backend_node_id: None,
            children,
        }
    }

    #[test]
    fn detects_429() {
        let tree = node(
            "RootWebArea",
            "GitHub",
            vec![node(
                "heading",
                "429 Too Many Requests",
                vec![node(
                    "paragraph",
                    "You have exceeded a secondary rate limit. Try again in 30 seconds.",
                    vec![],
                )],
            )],
        );
        let sig = detect(&tree).unwrap();
        assert_eq!(sig.kind, RateLimitKind::Http429);
        assert_eq!(sig.kind.default_backoff_secs(), 30);
    }

    #[test]
    fn detects_cloudflare_interstitial() {
        let tree = node(
            "RootWebArea",
            "Site",
            vec![node(
                "heading",
                "Checking your browser before accessing example.com",
                vec![node(
                    "paragraph",
                    "DDoS protection by Cloudflare. Ray ID: 8a1b2c3d4e5f.",
                    vec![],
                )],
            )],
        );
        let sig = detect(&tree).unwrap();
        assert_eq!(sig.kind, RateLimitKind::Cloudflare);
        assert_eq!(sig.kind.default_backoff_secs(), 15);
    }

    #[test]
    fn detects_generic_bot_detection() {
        let tree = node(
            "RootWebArea",
            "Search",
            vec![node(
                "heading",
                "Access denied",
                vec![node(
                    "paragraph",
                    "Our systems have detected unusual traffic from your network.",
                    vec![],
                )],
            )],
        );
        let sig = detect(&tree).unwrap();
        assert_eq!(sig.kind, RateLimitKind::Generic);
    }

    #[test]
    fn clean_page_returns_none() {
        let tree = node(
            "RootWebArea",
            "Home",
            vec![
                node("heading", "Welcome", vec![]),
                node("paragraph", "Browse our catalog of products.", vec![]),
            ],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn explicit_429_beats_cloudflare_when_both_present() {
        // Priority test: a 429 page that also mentions Cloudflare
        // is reported as 429 (more specific).
        let tree = node(
            "RootWebArea",
            "Site",
            vec![node(
                "paragraph",
                "HTTP 429. Cloudflare ray id: 1234. Too many requests.",
                vec![],
            )],
        );
        let sig = detect(&tree).unwrap();
        assert_eq!(sig.kind, RateLimitKind::Http429);
    }

    #[test]
    fn default_backoff_matrix() {
        // Lock the defaults down so a future tuning commit is
        // explicit.
        assert_eq!(RateLimitKind::Http429.default_backoff_secs(), 30);
        assert_eq!(RateLimitKind::Cloudflare.default_backoff_secs(), 15);
        assert_eq!(RateLimitKind::Generic.default_backoff_secs(), 10);
    }

    #[test]
    fn cross_module_composition_with_mock_fixtures() {
        // The rate-limit detector applied to the shared
        // `mock_fixtures` produces the expected per-page
        // results.
        use crate::mock_fixtures;

        // 429: Http429 kind.
        let r = detect(&mock_fixtures::http_429_page()).expect("429 should be detected");
        assert_eq!(r.kind, RateLimitKind::Http429);

        // Cloudflare: Cloudflare kind.
        let r = detect(&mock_fixtures::cloudflare_page()).expect("cloudflare should be detected");
        assert_eq!(r.kind, RateLimitKind::Cloudflare);

        // Cookie / login / age / newsletter / clean: no
        // rate-limit signal. The modal detector handles
        // these, not the rate-limit one.
        assert!(detect(&mock_fixtures::cookie_banner_page()).is_none());
        assert!(detect(&mock_fixtures::login_wall_page()).is_none());
        assert!(detect(&mock_fixtures::age_gate_page()).is_none());
        assert!(detect(&mock_fixtures::newsletter_popup_page()).is_none());
        assert!(detect(&mock_fixtures::clean_homepage()).is_none());
        assert!(detect(&mock_fixtures::dashboard_page()).is_none());
    }
}

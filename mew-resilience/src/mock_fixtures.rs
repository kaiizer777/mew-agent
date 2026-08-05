//! Phase 6 — Mock page fixtures.
//!
//! Synthetic `mew_perception::TreeNode` trees representing the
//! six failure-mode pages (cookie banner, login wall, newsletter,
//! age gate, 429, Cloudflare interstitial), one clean dashboard
//! (negative case), and one dashboard-turned-login (session
//! loss). Every fixture is a pure function with no I/O so the
//! unit tests in each module can use the same shape the agent
//! loop would see at runtime.
//!
//! The trees are deliberately small (5-15 nodes each). The
//! detectors are designed to be insensitive to tree size — a
//! cookie banner is a cookie banner whether the rest of the page
//! has 3 nodes or 3000. The small fixture is a feature: it's
//! what makes the tests fast and the failure modes reproducible.

use mew_perception::{NodeCategory, TreeNode};

/// Tiny helper: build a `TreeNode` with sensible defaults for the
/// fields the detectors don't read (category, value, backend_node_id).
/// Keeps the fixtures readable — the focus is on role + name + ref_id +
/// children, not the boilerplate.
fn n(role: &str, name: &str, children: Vec<TreeNode>) -> TreeNode {
    n_with_ref(role, name, None, children)
}

fn n_with_ref(role: &str, name: &str, ref_id: Option<&str>, children: Vec<TreeNode>) -> TreeNode {
    TreeNode {
        id: format!("n_{}_{}", role.replace(' ', "_"), name.replace(' ', "_")),
        role: role.to_string(),
        name: name.to_string(),
        value: String::new(),
        category: NodeCategory::Structural,
        ref_id: ref_id.map(|s| s.to_string()),
        // The detectors don't read `backend_node_id`; they key
        // on `ref_id` (the short `@eN` string). Leaving it
        // `None` is the safe default and keeps the fixtures
        // free of chromiumoxide re-exports.
        backend_node_id: None,
        children,
    }
}

/// A clean homepage. Negative case for the modal detector and the
/// rate-limit detector.
pub fn clean_homepage() -> TreeNode {
    n(
        "RootWebArea",
        "Acme",
        vec![
            n("heading", "Welcome to Acme", vec![]),
            n("link", "About", vec![n_with_ref("text", "About us", Some("@e1"), vec![])]),
            n("link", "Pricing", vec![n_with_ref("text", "See pricing", Some("@e2"), vec![])]),
        ],
    )
}

/// Cookie consent banner. Positive case for the cookie modal.
pub fn cookie_banner_page() -> TreeNode {
    n(
        "RootWebArea",
        "Acme",
        vec![
            n("heading", "Acme homepage", vec![]),
            n(
                "dialog",
                "Cookie consent",
                vec![
                    n("heading", "We use cookies", vec![]),
                    n(
                        "paragraph",
                        "We use cookies to improve your experience. Accept all?",
                        vec![],
                    ),
                    n_with_ref("button", "Accept all", Some("@e1"), vec![]),
                    n_with_ref("button", "Reject non-essential", Some("@e2"), vec![]),
                ],
            ),
        ],
    )
}

/// Login wall. Positive case for the login modal + the
/// session-loss detector (when the prior was a dashboard).
pub fn login_wall_page() -> TreeNode {
    n(
        "RootWebArea",
        "Acme",
        vec![
            n(
                "dialog",
                "Sign in to continue",
                vec![
                    n("heading", "Sign in", vec![]),
                    n_with_ref("textbox", "Email", Some("@e1"), vec![]),
                    n_with_ref("textbox", "Password", Some("@e2"), vec![]),
                    n_with_ref("button", "Sign in", Some("@e3"), vec![]),
                ],
            ),
        ],
    )
}

/// Newsletter popup. Positive case for the newsletter modal.
pub fn newsletter_popup_page() -> TreeNode {
    n(
        "RootWebArea",
        "Acme",
        vec![
            n("heading", "Today's deals", vec![]),
            n(
                "dialog",
                "Subscribe to our newsletter",
                vec![
                    n("paragraph", "Get 10% off your first order.", vec![]),
                    n_with_ref("button", "Subscribe", Some("@e1"), vec![]),
                    n_with_ref("button", "No thanks", Some("@e2"), vec![]),
                ],
            ),
        ],
    )
}

/// Age gate. Positive case for the age-gate modal.
pub fn age_gate_page() -> TreeNode {
    n(
        "RootWebArea",
        "WinesOnline",
        vec![
            n(
                "dialog",
                "Age verification",
                vec![
                    n("heading", "Are you 18 or older?", vec![]),
                    n_with_ref("button", "Yes, I am 18+", Some("@e1"), vec![]),
                    n_with_ref("button", "No, I am not", Some("@e2"), vec![]),
                ],
            ),
        ],
    )
}

/// HTTP 429 page (GitHub-style). Positive case for the
/// rate-limit detector's 429 branch.
pub fn http_429_page() -> TreeNode {
    n(
        "RootWebArea",
        "GitHub",
        vec![
            n("heading", "429 Too Many Requests", vec![]),
            n(
                "paragraph",
                "You have exceeded a secondary rate limit. Please wait 30 seconds and try again.",
                vec![],
            ),
        ],
    )
}

/// Cloudflare interstitial. Positive case for the rate-limit
/// detector's Cloudflare branch.
pub fn cloudflare_page() -> TreeNode {
    n(
        "RootWebArea",
        "Example",
        vec![
            n("heading", "Checking your browser before accessing example.com", vec![]),
            n(
                "paragraph",
                "DDoS protection by Cloudflare. Ray ID: 8a1b2c3d4e5f6g. Your IP: 1.2.3.4.",
                vec![],
            ),
        ],
    )
}

/// Dashboard page (prior state for the session-loss detector).
/// Returns the kind of tree you'd see *before* the user got
/// logged out.
pub fn dashboard_page() -> TreeNode {
    n(
        "RootWebArea",
        "Acme Dashboard",
        vec![
            n("heading", "Welcome back, Alice", vec![]),
            n(
                "table",
                "Recent activity",
                vec![
                    n_with_ref("row", "Today", Some("@e1"), vec![]),
                    n_with_ref("row", "Yesterday", Some("@e2"), vec![]),
                ],
            ),
            n_with_ref("link", "Settings", Some("@e3"), vec![]),
            n_with_ref("button", "Sign out", Some("@e4"), vec![]),
        ],
    )
}

/// Search page (the "this is *not* a session loss" negative case
/// for the session-loss detector when the prior was not a
/// dashboard).
pub fn search_page() -> TreeNode {
    n(
        "RootWebArea",
        "Help",
        vec![
            n("heading", "Search the help center", vec![]),
            n_with_ref("textbox", "Search", Some("@e1"), vec![]),
            n_with_ref("button", "Go", Some("@e2"), vec![]),
        ],
    )
}

/// Multi-modal page (cookie + newsletter on the same tree). Used
/// to test the modal detector's priority order.
pub fn multi_modal_page() -> TreeNode {
    n(
        "RootWebArea",
        "Acme",
        vec![
            n(
                "dialog",
                "Cookie consent",
                vec![n_with_ref("button", "Accept all", Some("@e1"), vec![])],
            ),
            n(
                "dialog",
                "Subscribe to our newsletter",
                vec![n_with_ref("button", "Subscribe", Some("@e2"), vec![])],
            ),
        ],
    )
}

// -------------------------------------------------------------------------
// Phase 8 fixtures: challenge / CAPTCHA pages (one per ChallengeKind)
// -------------------------------------------------------------------------
//
// The captcha detector is in `mew_resilience::captcha`. Each fixture
// below is the minimum tree the detector's heuristic would encounter
// in a real page: a `RootWebArea` whose `name` carries the real-site
// title (and often the domain) plus one or more children with the
// challenge's identifying text or class.

/// Cloudflare Turnstile challenge page. Positive case for
/// `ChallengeKind::CloudflareTurnstile`.
pub fn cloudflare_turnstile_page() -> TreeNode {
    n(
        "RootWebArea",
        "instagram.com",
        vec![
            n("heading", "instagram.com", vec![]),
            n("paragraph", "Verify you are human", vec![]),
            n_with_ref(
                "iframe",
                "cf-turnstile",
                Some("@e1"),
                vec![],
            ),
        ],
    )
}

/// reCAPTCHA v2 challenge page. Positive case for
/// `ChallengeKind::RecaptchaV2`.
pub fn recaptcha_v2_page() -> TreeNode {
    n(
        "RootWebArea",
        "Example Login",
        vec![
            n("heading", "Example Login", vec![]),
            n("paragraph", "I am not a robot", vec![]),
            n_with_ref(
                "iframe",
                "www.google.com/recaptcha/api2",
                Some("@e1"),
                vec![],
            ),
        ],
    )
}

/// reCAPTCHA v3 (invisible). Positive case for
/// `ChallengeKind::RecaptchaV3`.
pub fn recaptcha_v3_page() -> TreeNode {
    n(
        "RootWebArea",
        "Example",
        vec![
            n(
                "script",
                "https://www.google.com/recaptcha/api.js?render=explicit",
                vec![],
            ),
            n("div", "grecaptcha-badge", vec![]),
        ],
    )
}

/// hCaptcha challenge page. Positive case for
/// `ChallengeKind::Hcaptcha`.
pub fn hcaptcha_page() -> TreeNode {
    n(
        "RootWebArea",
        "Example",
        vec![n_with_ref(
            "iframe",
            "hcaptcha.com/1/api",
            Some("@e1"),
            vec![],
        )],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_homepage_has_no_modal_signal() {
        // Sanity: the negative case is what every detector should
        // be *insensitive* to. If this fixture starts tripping a
        // detector, that's a regression in the detector, not the
        // fixture.
        use crate::modal_interrupts::detect as detect_modal;
        use crate::rate_limit::detect as detect_rate;
        use crate::captcha::detect as detect_captcha;
        assert!(detect_modal(&clean_homepage()).is_none());
        assert!(detect_rate(&clean_homepage()).is_none());
        // Phase 8: the captcha detector also treats a normal
        // homepage as negative.
        assert!(detect_captcha(&clean_homepage()).is_none());
    }

    #[test]
    fn captcha_fixtures_round_trip_via_clone() {
        // The captcha fixtures must be Clone-able so the
        // detector tests can share them.
        let f = cloudflare_turnstile_page();
        let _ = f.clone();
        let _ = recaptcha_v2_page().clone();
        let _ = recaptcha_v3_page().clone();
        let _ = hcaptcha_page().clone();
    }

    #[test]
    fn captcha_fixtures_match_their_expected_kinds() {
        // Cross-check: each captcha fixture must trigger the
        // captcha detector with the expected kind, so a
        // future fixture change is caught.
        use crate::captcha::{detect as detect_captcha, ChallengeKind};
        let c = detect_captcha(&cloudflare_turnstile_page()).expect("turnstile page");
        assert_eq!(c.kind, ChallengeKind::CloudflareTurnstile);
        let c = detect_captcha(&recaptcha_v2_page()).expect("v2 page");
        assert_eq!(c.kind, ChallengeKind::RecaptchaV2);
        let c = detect_captcha(&recaptcha_v3_page()).expect("v3 page");
        assert_eq!(c.kind, ChallengeKind::RecaptchaV3);
        let c = detect_captcha(&hcaptcha_page()).expect("hcaptcha page");
        assert_eq!(c.kind, ChallengeKind::Hcaptcha);
    }

    #[test]
    fn non_captcha_fixtures_dont_trigger_captcha_detector() {
        // Cross-check: the cookie / login / 429 / cloudflare
        // interstitial / age-gate / newsletter / dashboard
        // fixtures must NOT trigger the captcha detector.
        // Otherwise a future detector widening would silently
        // start pausing on the wrong pages.
        use crate::captcha::detect as detect_captcha;
        for f in [
            clean_homepage(),
            cookie_banner_page(),
            login_wall_page(),
            newsletter_popup_page(),
            age_gate_page(),
            http_429_page(),
            cloudflare_page(),
            dashboard_page(),
            search_page(),
            multi_modal_page(),
        ] {
            assert!(detect_captcha(&f).is_none(), "unexpected captcha on fixture");
        }
    }

    #[test]
    fn fixtures_round_trip_via_clone() {
        // All fixtures must be Clone-able so they can be shared
        // across multiple detector tests in a single test run.
        let f = cookie_banner_page();
        let _ = f.clone();
    }

    #[test]
    fn all_fixtures_build_without_panic() {
        // Smoke test: every public fixture must build. If a new
        // fixture is added without a corresponding test, this
        // catch-all ensures it at least *exists* and is wired in.
        let _ = clean_homepage();
        let _ = cookie_banner_page();
        let _ = login_wall_page();
        let _ = newsletter_popup_page();
        let _ = age_gate_page();
        let _ = http_429_page();
        let _ = cloudflare_page();
        let _ = dashboard_page();
        let _ = search_page();
        let _ = multi_modal_page();
        // Phase 8 challenge fixtures.
        let _ = cloudflare_turnstile_page();
        let _ = recaptcha_v2_page();
        let _ = recaptcha_v3_page();
        let _ = hcaptcha_page();
    }
}

//! Phase 7 — Account-restricted / unusual-activity page detector.
//!
//! Background: job boards and freelance platforms (LinkedIn,
//! Indeed, Upwork, etc.) have a separate anti-abuse surface
//! from rate-limit pages: when their risk model flags a
//! session, the user lands on an "We've restricted your
//! account because of unusual activity" page with a
//! challenge to verify identity, a request to wait X hours,
//! or a redirect to support. This is *not* a CAPTCHA (no
//! challenge to solve) and *not* a rate-limit 429 (the
//! platform is deliberately throttling *this user*, not
//! refusing the request), but it is a hard stop for the
//! agent: clicking through the page would risk permanent
//! account lock-out.
//!
//! The fix: detect this page shape in the AX tree and
//! surface it as a typed `ResilienceFinding` so the agent
//! pauses with a clear, non-alarming message. The right
//! user action is "go to LinkedIn / Indeed in a real
//! browser, verify your identity, come back" — and the
//! message must be calm because the user did nothing
//! wrong, automated agent activity just tripped a
//! generic risk threshold.
//!
//! Detection is heuristic on the *body text* of the page.
//! Recognized shapes (all string-substring, lowercased):
//!
//!   * "We've restricted your account" (LinkedIn)
//!   * "Account restricted" (generic)
//!   * "Your account has been temporarily restricted" (Upwork)
//!   * "unusual activity from your account" (LinkedIn)
//!   * "unusual login activity" (LinkedIn)
//!   * "verify your identity" + "account" (generic verify flow)
//!   * "unusual traffic from your network" — already
//!     covered by the rate-limit detector's "are you a bot"
//!     signal; the account-restriction detector does *not*
//!     overlap it because account restriction is per-user
//!     and rate-limit is per-IP. We keep them disjoint.
//!
//! Pure-Rust, no I/O. Tests cover the canonical shapes plus
//! the negative case (a normal page) and a false-positive
//! guard (a page that mentions "restricted" in a
//! non-account context, e.g. a network "restricted to
//! members" paywall).

use mew_perception::TreeNode;

/// What kind of account-restriction signal the detector saw.
/// Mirrors `RateLimitKind`'s shape so a future caller can
/// branch on the kind if it wants different copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountRestrictionKind {
    /// LinkedIn's canonical "We've restricted your account"
    /// page. Tells the user to verify their identity or
    /// wait. This is the most common shape the agent
    /// encounters in production.
    LinkedInRestricted,
    /// Upwork / Contra / generic freelance board's
    /// "Your account has been temporarily restricted"
    /// page. Often includes a wait timer ("try again in
    /// 24 hours") and a support contact.
    FreelanceBoardRestricted,
    /// Generic shape: "Account restricted" or "unusual
    /// activity" without a specific platform signature.
    Generic,
}

impl AccountRestrictionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountRestrictionKind::LinkedInRestricted => "linkedin_restricted",
            AccountRestrictionKind::FreelanceBoardRestricted => "freelance_restricted",
            AccountRestrictionKind::Generic => "generic_account_restriction",
        }
    }
}

/// The detector's verdict. `reason` is the user-facing
/// short message ("LinkedIn restricted your account
/// because of unusual activity") and `hint` is the
/// actionable instruction ("Sign in to LinkedIn in a real
/// browser, verify your identity, and the agent can
/// continue."). The split lets the resilience adapter
/// render different surfaces (e.g. a transcript log
/// uses `reason`, the chat-visible progress line uses
/// `hint`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRestrictionSignal {
    pub kind: AccountRestrictionKind,
    pub reason: String,
    pub hint: String,
}

/// Scan the tree for the account-restriction signal. The
/// detector walks the tree depth-first and concatenates the
/// names of all text-bearing nodes into a single lowercase
/// blob, then runs the pattern match. Concatenation is
/// bounded (we stop after N characters) to avoid pathological
/// cost on huge trees.
///
/// The first match wins. We classify in priority order:
/// LinkedIn > FreelanceBoard > Generic. The patterns are
/// chosen to be platform-specific so a false positive on
/// "restricted" (e.g. a paywall) is unlikely.
pub fn detect(tree: &TreeNode) -> Option<AccountRestrictionSignal> {
    const MAX_TEXT_BYTES: usize = 8192;
    let mut buf = String::new();
    collect_text(tree, &mut buf, MAX_TEXT_BYTES);
    let lower = buf.to_lowercase();

    // LinkedIn's canonical pattern. The page typically
    // includes "We've restricted your account" plus a
    // "verify your identity" follow-up; the former is
    // specific enough to identify the platform.
    if lower.contains("we've restricted your account")
        || lower.contains("we have restricted your account")
        || lower.contains("your account has been restricted")
            && lower.contains("linkedin")
    {
        return Some(AccountRestrictionSignal {
            kind: AccountRestrictionKind::LinkedInRestricted,
            reason: "LinkedIn has restricted this account because of unusual activity".to_string(),
            hint:
                "Sign in to LinkedIn in a real browser, complete the identity verification, and try again later. The agent has paused to protect your account."
                    .to_string(),
        });
    }

    // LinkedIn's "unusual login activity" page, which is
    // distinct from the "restricted" page (the former
    // asks for a verification code, the latter is a
    // hard stop). Both fall into the same detector bucket
    // because the user action is identical.
    if lower.contains("unusual login activity")
        && (lower.contains("linkedin") || lower.contains("account"))
    {
        return Some(AccountRestrictionSignal {
            kind: AccountRestrictionKind::LinkedInRestricted,
            reason: "LinkedIn detected unusual login activity on this account".to_string(),
            hint:
                "Sign in to LinkedIn in a real browser, confirm the login was you, and try again. The agent has paused to protect your account."
                    .to_string(),
        });
    }

    // Upwork / Contra / generic freelance board.
    // The "temporarily restricted" wording is the
    // common shape across these platforms.
    if lower.contains("your account has been temporarily restricted")
        || lower.contains("account temporarily restricted")
    {
        return Some(AccountRestrictionSignal {
            kind: AccountRestrictionKind::FreelanceBoardRestricted,
            reason: "Freelance board has temporarily restricted this account".to_string(),
            hint:
                "Sign in to the platform in a real browser, wait for the restriction to lift (often 24 hours), or contact support. The agent has paused to protect your account."
                    .to_string(),
        });
    }

    // Generic "account restricted" or "unusual activity"
    // — broader match for platforms we don't have a
    // specific pattern for. Kept last so platform-specific
    // matches win when both are present.
    if (lower.contains("account restricted") || lower.contains("account has been restricted"))
        && !lower.contains("members only")
        && !lower.contains("restricted to")
    {
        return Some(AccountRestrictionSignal {
            kind: AccountRestrictionKind::Generic,
            reason: "Platform reported an account restriction".to_string(),
            hint:
                "Sign in to the platform in a real browser, resolve the restriction, and try again later. The agent has paused to protect your account."
                    .to_string(),
        });
    }

    if lower.contains("unusual activity from your account") {
        return Some(AccountRestrictionSignal {
            kind: AccountRestrictionKind::Generic,
            reason: "Platform flagged unusual activity on this account".to_string(),
            hint:
                "Sign in to the platform in a real browser, confirm the activity was you, and try again. The agent has paused to protect your account."
                    .to_string(),
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
            id: format!("n_{}_{}", role.replace(' ', "_"), name.replace(' ', "_")),
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
    fn detects_linkedin_restricted_account() {
        let tree = node(
            "RootWebArea",
            "LinkedIn",
            vec![
                node("heading", "We've restricted your account", vec![]),
                node(
                    "paragraph",
                    "We've restricted your account because of unusual activity. Please verify your identity to continue.",
                    vec![],
                ),
                node("button", "Verify identity", vec![]),
            ],
        );
        let sig = detect(&tree).expect("LinkedIn restricted page must be detected");
        assert_eq!(sig.kind, AccountRestrictionKind::LinkedInRestricted);
        assert!(sig.reason.to_lowercase().contains("linkedin"));
        assert!(sig.hint.to_lowercase().contains("sign in"));
    }

    #[test]
    fn detects_linkedin_unusual_login_activity() {
        let tree = node(
            "RootWebArea",
            "LinkedIn",
            vec![
                node("heading", "Unusual login activity", vec![]),
                node(
                    "paragraph",
                    "We noticed some unusual login activity on your LinkedIn account. Was this you?",
                    vec![],
                ),
            ],
        );
        let sig = detect(&tree).expect("LinkedIn unusual login must be detected");
        assert_eq!(sig.kind, AccountRestrictionKind::LinkedInRestricted);
    }

    #[test]
    fn detects_upwork_temporary_restriction() {
        let tree = node(
            "RootWebArea",
            "Upwork",
            vec![
                node("heading", "Account temporarily restricted", vec![]),
                node(
                    "paragraph",
                    "Your account has been temporarily restricted. Please try again in 24 hours or contact support.",
                    vec![],
                ),
            ],
        );
        let sig = detect(&tree).expect("Upwork temporary restriction must be detected");
        assert_eq!(
            sig.kind,
            AccountRestrictionKind::FreelanceBoardRestricted
        );
    }

    #[test]
    fn detects_generic_account_restricted() {
        let tree = node(
            "RootWebArea",
            "Example Job Board",
            vec![
                node("heading", "Account restricted", vec![]),
                node(
                    "paragraph",
                    "Your account has been restricted due to violations of our terms of service.",
                    vec![],
                ),
            ],
        );
        let sig = detect(&tree).expect("generic restriction must be detected");
        assert_eq!(sig.kind, AccountRestrictionKind::Generic);
    }

    #[test]
    fn detects_unusual_activity_from_account() {
        let tree = node(
            "RootWebArea",
            "Indeed",
            vec![node(
                "paragraph",
                "We've detected unusual activity from your account. Please verify it's you.",
                vec![],
            )],
        );
        let sig = detect(&tree).expect("unusual activity must be detected");
        assert_eq!(sig.kind, AccountRestrictionKind::Generic);
    }

    #[test]
    fn does_not_detect_paywall() {
        // False positive guard: a "members only" / paywall
        // page is not an account restriction — the user
        // hasn't been blocked, they just need to pay. The
        // detector must not fire on this shape.
        let tree = node(
            "RootWebArea",
            "News Site",
            vec![
                node("heading", "Subscribe to continue", vec![]),
                node(
                    "paragraph",
                    "This article is restricted to members. Subscribe to read more.",
                    vec![],
                ),
                node("button", "Subscribe", vec![]),
            ],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn does_not_detect_clean_dashboard() {
        let tree = node(
            "RootWebArea",
            "Acme",
            vec![
                node("heading", "Welcome back", vec![]),
                node("paragraph", "Browse the latest jobs.", vec![]),
            ],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn does_not_detect_captcha_page() {
        // A CAPTCHA page is *not* an account restriction.
        // The two detectors are disjoint: CAPTCHA means
        // "prove you're human", restriction means "we
        // know you're human but your behavior was bad".
        let tree = node(
            "RootWebArea",
            "Example",
            vec![
                node("paragraph", "Verify you are human", vec![]),
                node("iframe", "cf-turnstile", vec![]),
            ],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn cross_module_composition_with_mock_fixtures() {
        // The account-restriction detector applied to the
        // shared `mock_fixtures` produces the expected
        // per-page results. Existing fixtures (cookie
        // banner, 429, cloudflare, captcha, clean
        // homepage) must NOT trip this detector — those
        // are independent failure modes.
        use crate::mock_fixtures;
        assert!(detect(&mock_fixtures::clean_homepage()).is_none());
        assert!(detect(&mock_fixtures::cookie_banner_page()).is_none());
        assert!(detect(&mock_fixtures::login_wall_page()).is_none());
        assert!(detect(&mock_fixtures::http_429_page()).is_none());
        assert!(detect(&mock_fixtures::cloudflare_page()).is_none());
        assert!(detect(&mock_fixtures::cloudflare_turnstile_page()).is_none());
        assert!(detect(&mock_fixtures::recaptcha_v2_page()).is_none());
        assert!(detect(&mock_fixtures::dashboard_page()).is_none());
    }
}

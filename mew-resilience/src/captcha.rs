//! Phase 8 — Failure mode 7: Challenge / CAPTCHA page detection.
//!
//! Background: the existing rate-limit detector catches the *easy*
//! cases — a literal "429 Too Many Requests" page or a Cloudflare
//! interstitial. Those are server-side refusal. The harder cases
//! are *client-side challenges* — Cloudflare Turnstile, reCAPTCHA
//! v2/v3, hCaptcha — where the page looks superficially normal
//! but contains an embedded iframe or widget that the agent
//! cannot interact with through a normal `click(@eN)` because
//! the challenge is rendered in a cross-origin iframe.
//!
//! Real-world 2026 consensus (see the work.md Phase 8 background
//! links): avoidance-first, not solving. The agent's *default*
//! response to a challenge is to pause and hand off to the user,
//! because mew runs a real visible headed browser and the human
//! can solve it in-window. A third-party solving-service
//! integration is available as an *opt-in* flag for users who
//! explicitly want unattended runs, with a documented
//! cost / ToS caveat (see `mew_agent::AgentConfig::captcha`).
//!
//! Detection is heuristic on the *text + role* of the rendered
//! tree. We recognize four families:
//!
//!   * `CloudflareTurnstile` — the page contains a Turnstile
//!     iframe (`challenges.cloudflare.com/turnstile`) and/or the
//!     text "Verify you are human" / "Checking your browser".
//!   * `RecaptchaV2`        — the page contains a reCAPTCHA v2
//!     iframe (`google.com/recaptcha`) and/or the literal
//!     "I am not a robot" checkbox.
//!   * `RecaptchaV3`        — invisible; detected via the
//!     reCAPTCHA v3 script tag (`grecaptcha.execute`) or the
//!     `grecaptcha-badge` DOM node.
//!   * `Hcaptcha`           — the page contains an hCaptcha
//!     iframe (`hcaptcha.com`) and/or an "hcaptcha-challenge"
//!     class.
//!
//! Pure-Rust, no I/O, no LLM. Tests cover all four families plus
//! the clean-page negative case. The detector returns a typed
//! `Challenge` value the resilience adapter maps to a
//! `ResilienceHookOutcome::PauseForCaptcha` (or a no-op for the
//! never-pause case).

use mew_perception::TreeNode;
use serde::{Deserialize, Serialize};

/// The kind of challenge the detector saw. The string form is
/// what the user sees in the chat list ("a Cloudflare Turnstile
/// challenge was detected on the page"); the orchestrator / UI
/// uses it to render the right "please solve X" wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChallengeKind {
    /// Cloudflare Turnstile (the modern 2022+ replacement for
    /// the older "I'm Under Attack" mode).
    CloudflareTurnstile,
    /// Google reCAPTCHA v2 ("I am not a robot" checkbox + image
    /// grid).
    RecaptchaV2,
    /// Google reCAPTCHA v3 (invisible / score-based).
    RecaptchaV3,
    /// hCaptcha (the privacy-focused reCAPTCHA alternative; many
    /// Cloudflare-protected sites use it).
    Hcaptcha,
}

impl ChallengeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChallengeKind::CloudflareTurnstile => "cloudflare_turnstile",
            ChallengeKind::RecaptchaV2 => "recaptcha_v2",
            ChallengeKind::RecaptchaV3 => "recaptcha_v3",
            ChallengeKind::Hcaptcha => "hcaptcha",
        }
    }

    /// User-facing display name. Short, capitalized, fits in a
    /// chat list line without truncation.
    pub fn display_name(self) -> &'static str {
        match self {
            ChallengeKind::CloudflareTurnstile => "Cloudflare Turnstile",
            ChallengeKind::RecaptchaV2 => "reCAPTCHA v2",
            ChallengeKind::RecaptchaV3 => "reCAPTCHA v3",
            ChallengeKind::Hcaptcha => "hCaptcha",
        }
    }

    /// Short user-facing explanation of what the user needs to
    /// do. Used in the chat list message that fires when the
    /// challenge is detected. Kept generic enough to be true for
    /// both first-time and returning users.
    pub fn user_action_hint(self) -> &'static str {
        match self {
            ChallengeKind::CloudflareTurnstile => {
                "The agent has paused. Please click the \"I'm not a robot\" box or solve the challenge in the browser window, then tell the agent to continue."
            }
            ChallengeKind::RecaptchaV2 => {
                "The agent has paused. Please tick the \"I'm not a robot\" checkbox and solve the image challenge in the browser window, then tell the agent to continue."
            }
            ChallengeKind::RecaptchaV3 => {
                "The agent has paused. An invisible reCAPTCHA v3 check is running. If a checkbox appears, please complete it; otherwise the page should auto-resolve."
            }
            ChallengeKind::Hcaptcha => {
                "The agent has paused. Please solve the hCaptcha challenge in the browser window, then tell the agent to continue."
            }
        }
    }
}

/// The detector's verdict. `domain_hint` is the host the
/// detector saw the challenge on (e.g. `"www.instagram.com"`) —
/// the agent uses it to log a per-domain telemetry event
/// (Phase 8.5) and to look up the `known_to_challenge_bots`
/// flag on the sensitive-platform table (Phase 8.4). When the
/// detector cannot infer a domain, `domain_hint` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub kind: ChallengeKind,
    /// The host the challenge was on, when inferable from the
    /// tree. The detector looks at the root `RootWebArea`
    /// node's `name` (which usually contains the site title
    /// and sometimes the domain) and at any visible
    /// `<address>`-like text. This is a *hint*; the resilience
    /// adapter is free to override it with the page's actual
    /// URL when it has that available.
    pub domain_hint: Option<String>,
    /// Short label the resilience adapter uses in the
    /// transcript log and the chat reply. Includes the host
    /// when known so a reviewer can correlate the finding with
    /// the page that produced it.
    pub label: String,
}

/// Walk the tree and detect any of the four challenge families.
/// Returns the *first* match. We classify in a fixed priority
/// order: Cloudflare Turnstile > reCAPTCHA v2 > reCAPTCHA v3 >
/// hCaptcha. The order matters when a page embeds more than one
/// (rare but possible — Cloudflare Turnstile sometimes wraps a
/// reCAPTCHA on the same page); the user-facing "please solve
/// this" message is more useful when it points at the more
/// visible widget.
///
/// The tree is walked in a single pass with a bounded text
/// concatenation (8 KiB cap, mirroring the rate-limit detector
/// in `rate_limit.rs`) so a huge tree doesn't blow up the
/// detector's CPU. The text blob is run against a small set of
/// substrings.
pub fn detect(tree: &TreeNode) -> Option<Challenge> {
    const MAX_TEXT_BYTES: usize = 8192;
    let mut buf = String::new();
    collect_text(tree, &mut buf, MAX_TEXT_BYTES);
    let lower = buf.to_lowercase();
    let domain_hint = extract_domain_hint(tree);

    // Cloudflare Turnstile. The "verify you are human" string
    // is shared with the older Cloudflare interstitial, but
    // Turnstile adds the iframe + the explicit
    // `cf-turnstile` class hook. Both signals are checked
    // because some pages render the iframe inside a shadow
    // root and only the visible text reaches the tree.
    if lower.contains("cf-turnstile")
        || lower.contains("challenges.cloudflare.com/turnstile")
        || lower.contains("turnstile")
            && (lower.contains("verify you are human")
                || lower.contains("checking your browser"))
    {
        return Some(Challenge {
            kind: ChallengeKind::CloudflareTurnstile,
            domain_hint: domain_hint.clone(),
            label: format_challenge_label("Cloudflare Turnstile", &domain_hint),
        });
    }

    // reCAPTCHA v2. The "i am not a robot" string is the
    // canonical visible cue; the iframe URL is a back-up for
    // the case where the iframe renders in a shadow root and
    // the visible text doesn't make it into the tree.
    if lower.contains("i am not a robot")
        || lower.contains("recaptcha/api2")
        || lower.contains("g-recaptcha")
    {
        return Some(Challenge {
            kind: ChallengeKind::RecaptchaV2,
            domain_hint: domain_hint.clone(),
            label: format_challenge_label("reCAPTCHA v2", &domain_hint),
        });
    }

    // reCAPTCHA v3. Invisible — no checkbox — so the only
    // signals are the script + the badge. Both are checked
    // because v3 sites may inject the script but not render
    // the badge UI.
    if lower.contains("recaptcha/v3")
        || lower.contains("grecaptcha.execute")
        || lower.contains("grecaptcha-badge")
    {
        return Some(Challenge {
            kind: ChallengeKind::RecaptchaV3,
            domain_hint: domain_hint.clone(),
            label: format_challenge_label("reCAPTCHA v3", &domain_hint),
        });
    }

    // hCaptcha. The hcaptcha.com iframe URL is the strongest
    // signal; the `hcaptcha-challenge` class is the fallback
    // for shadow-rooted renders.
    if lower.contains("hcaptcha.com/1/api")
        || lower.contains("hcaptcha-challenge")
        || lower.contains("h-captcha")
    {
        let label = format_challenge_label("hCaptcha", &domain_hint);
        return Some(Challenge {
            kind: ChallengeKind::Hcaptcha,
            domain_hint,
            label,
        });
    }

    None
}

/// Build the user-facing label for the chat list / transcript.
/// Includes the host when known so the user can tell *which*
/// site needs their attention (the agent can be on instagram
/// while the LLM is describing something else).
fn format_challenge_label(kind_display: &str, domain_hint: &Option<String>) -> String {
    match domain_hint {
        Some(host) => format!("{kind_display} challenge detected on {host}"),
        None => format!("{kind_display} challenge detected on this page"),
    }
}

/// Best-effort: pull a host-looking string out of the root
/// `RootWebArea` node's name. Most challenge pages are
/// chrome on top of the real site, so the root's
/// accessible name usually still includes the real site's
/// title (and sometimes its host). Returns `None` when the
/// tree doesn't yield anything host-shaped — the resilience
/// adapter is free to override with the page's actual URL.
fn extract_domain_hint(tree: &TreeNode) -> Option<String> {
    // Look for the root node (role `RootWebArea` or any node
    // with no parent). For our purposes, the first node we
    // walk that has a non-empty name is a fine proxy for the
    // root. We extract any substring that looks like
    // `foo.bar` (two or more labels, at least one dot, ASCII
    // alphanumeric + hyphens).
    fn walk(n: &TreeNode, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        if let Some(host) = first_host_like_substring(&n.name) {
            *out = Some(host);
            return;
        }
        if let Some(host) = first_host_like_substring(&n.value) {
            *out = Some(host);
            return;
        }
        for child in &n.children {
            walk(child, out);
            if out.is_some() {
                return;
            }
        }
    }
    let mut out = None;
    walk(tree, &mut out);
    out
}

/// Return the first `foo.bar` substring in `s`, lowercased
/// and stripped of a leading `www.`. Conservatively only
/// matches shapes a real host can take: at least two
/// labels, each `[a-z0-9-]+`, the last label 2-24 chars
/// (the typical TLD length). Avoids matching natural-language
/// sentences that happen to contain a dot.
///
/// Greedy: when the next token is another `.<label>`
/// followed by a TLD-shaped segment, extend through it
/// (e.g. `www.example.com` matches as `www.example.com`,
/// not `www.example`).
fn first_host_like_substring(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Start of a candidate: an alphanumeric char.
        if !bytes[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        // Walk forward through the first label's chars.
        let label_start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-')
        {
            i += 1;
        }
        // Need a '.' and another label to call this a host.
        if i >= bytes.len()
            || bytes[i] != b'.'
            || i + 1 >= bytes.len()
            || !bytes[i + 1].is_ascii_alphanumeric()
        {
            continue;
        }
        // Walk label2.
        let second_start = i + 1;
        let mut j = second_start;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
            j += 1;
        }
        let second_end = j;
        let second_len = second_end - second_start;
        if !(2..=24).contains(&second_len) {
            // TLD too short / too long. Move past the dot
            // and keep scanning.
            i = second_end;
            continue;
        }
        // At this point we have a 2-label candidate.
        // Greedily extend through additional `<label>.tld`
        // pairs so the returned host is the full FQDN.
        let mut host_end = second_end;
        loop {
            // Need a `.<label>` continuation where
            // `<label>` is the TLD (2..=24 alnum/hyphen
            // chars) and the next char is a non-label
            // terminator (end-of-string, whitespace, or
            // `/`).
            if host_end >= bytes.len() || bytes[host_end] != b'.' {
                break;
            }
            let tld_start = host_end + 1;
            if tld_start >= bytes.len() || !bytes[tld_start].is_ascii_alphanumeric() {
                break;
            }
            let mut m = tld_start;
            while m < bytes.len() && (bytes[m].is_ascii_alphanumeric() || bytes[m] == b'-') {
                m += 1;
            }
            let tld_end = m;
            let tld_len = tld_end - tld_start;
            if !(2..=24).contains(&tld_len) {
                break;
            }
            // TLD must be followed by a non-label char so
            // we don't extend into a longer URL path.
            if tld_end < bytes.len()
                && (bytes[tld_end].is_ascii_alphanumeric() || bytes[tld_end] == b'-')
            {
                break;
            }
            host_end = tld_end;
        }
        let host = &lower[label_start..host_end];
        let host = host.strip_prefix("www.").unwrap_or(host);
        let first_label = host.split('.').next().unwrap_or("");
        if !first_label.is_empty() {
            return Some(host.to_string());
        }
        // Defensive fallthrough: continue scanning.
        i = second_end;
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
    fn clean_homepage_does_not_trigger() {
        let tree = node(
            "RootWebArea",
            "Acme",
            vec![
                node("heading", "Welcome to Acme", vec![]),
                node("paragraph", "Browse our products.", vec![]),
            ],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn detects_cloudflare_turnstile() {
        let tree = node(
            "RootWebArea",
            "instagram.com",
            vec![
                node("heading", "instagram.com", vec![]),
                node("paragraph", "Verify you are human", vec![]),
                node("iframe", "cf-turnstile", vec![]),
            ],
        );
        let c = detect(&tree).expect("turnstile must be detected");
        assert_eq!(c.kind, ChallengeKind::CloudflareTurnstile);
        assert_eq!(c.domain_hint.as_deref(), Some("instagram.com"));
        assert!(c.label.contains("Cloudflare"));
    }

    #[test]
    fn detects_recaptcha_v2() {
        let tree = node(
            "RootWebArea",
            "Example Login",
            vec![
                node("heading", "Example Login", vec![]),
                node("paragraph", "I am not a robot", vec![]),
                node("iframe", "www.google.com/recaptcha/api2", vec![]),
            ],
        );
        let c = detect(&tree).expect("recaptcha v2 must be detected");
        assert_eq!(c.kind, ChallengeKind::RecaptchaV2);
    }

    #[test]
    fn detects_recaptcha_v3() {
        // v3 is invisible — no checkbox text. The signals are
        // the v3 script URL and the badge node.
        let tree = node(
            "RootWebArea",
            "Example",
            vec![
                node("script", "https://www.google.com/recaptcha/api.js?render=explicit", vec![]),
                node("div", "grecaptcha-badge", vec![]),
            ],
        );
        let c = detect(&tree).expect("recaptcha v3 must be detected");
        assert_eq!(c.kind, ChallengeKind::RecaptchaV3);
    }

    #[test]
    fn detects_hcaptcha() {
        let tree = node(
            "RootWebArea",
            "Example",
            vec![
                node("iframe", "hcaptcha.com/1/api", vec![]),
            ],
        );
        let c = detect(&tree).expect("hcaptcha must be detected");
        assert_eq!(c.kind, ChallengeKind::Hcaptcha);
    }

    #[test]
    fn priority_cloudflare_turnstile_beats_recaptcha() {
        // When both Turnstile and reCAPTCHA appear in the same
        // tree (Cloudflare can wrap a reCAPTCHA), the more
        // visible / user-facing challenge wins.
        let tree = node(
            "RootWebArea",
            "Example",
            vec![
                node("iframe", "cf-turnstile", vec![]),
                node("paragraph", "I am not a robot", vec![]),
            ],
        );
        let c = detect(&tree).expect("a challenge must be detected");
        assert_eq!(c.kind, ChallengeKind::CloudflareTurnstile);
    }

    #[test]
    fn challenge_kind_as_str_is_stable() {
        // These strings end up in the transcript and the
        // tracing event payload. They are the user-visible
        // kind name and must not change without a coordinated
        // update of the frontend.
        assert_eq!(ChallengeKind::CloudflareTurnstile.as_str(), "cloudflare_turnstile");
        assert_eq!(ChallengeKind::RecaptchaV2.as_str(), "recaptcha_v2");
        assert_eq!(ChallengeKind::RecaptchaV3.as_str(), "recaptcha_v3");
        assert_eq!(ChallengeKind::Hcaptcha.as_str(), "hcaptcha");
    }

    #[test]
    fn user_action_hint_is_non_empty_for_every_kind() {
        for k in [
            ChallengeKind::CloudflareTurnstile,
            ChallengeKind::RecaptchaV2,
            ChallengeKind::RecaptchaV3,
            ChallengeKind::Hcaptcha,
        ] {
            let h = k.user_action_hint();
            assert!(!h.trim().is_empty(), "kind {:?} returned empty hint", k);
        }
    }

    #[test]
    fn host_like_substring_extracts_real_hosts() {
        // Positive cases.
        assert_eq!(
            first_host_like_substring("Welcome to instagram.com"),
            Some("instagram.com".to_string())
        );
        assert_eq!(
            first_host_like_substring("Visit www.example.com for more"),
            Some("example.com".to_string())
        );
        assert_eq!(
            first_host_like_substring("reCAPTCHA on www.google.com/recaptcha/api2"),
            Some("google.com".to_string())
        );
        // Negative cases — natural language, no real host.
        assert_eq!(first_host_like_substring("This is a sentence."), None);
        assert_eq!(first_host_like_substring("a.b"), None); // TLD too short
        assert_eq!(first_host_like_substring("e.g. something"), None);
    }
}

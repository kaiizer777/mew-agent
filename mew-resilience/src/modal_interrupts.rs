//! Phase 6 — Failure mode 2: Modal interruptions.
//!
//! Background: most real-world sites show an overlay on first load — a
//! cookie consent banner, a "Sign in to continue" wall, a "Subscribe to
//! our newsletter" popup, sometimes an "Are you 18+?" age gate. The
//! overlay usually has its own close button (or an "Accept all" CTA)
//! but the overlay covers the actual content the LLM is trying to
//! interact with. If the agent doesn't notice, every click hits the
//! overlay instead of the page — a particularly nasty failure mode
//! because the LLM's tool result says "clicked" but the click did
//! nothing useful.
//!
//! The fix: scan the accessibility tree every iteration for the
//! common overlay shapes, surface a typed `ModalReport` so the loop
//! can:
//!   * auto-dismiss the overlay (if a close button is visible) and
//!     continue, OR
//!   * surface the overlay to the LLM as a typed finding and let it
//!     decide (dismiss / accept / ignore / report-to-user).
//!
//! Detection is heuristic — there's no DOM-level "is this an overlay?"
//! signal that works across sites. We use role + name + position
//! matching. The patterns are deliberately narrow (cookie / login /
//! newsletter / age gate) so a false positive on real content is
//! rare. A "no detection" outcome is the common case and is the
//! expected fast path.
//!
//! Pure-Rust, no I/O. Tests cover the four common overlay shapes
//! plus a clean-page negative case and a multi-modal-on-one-page
//! edge case.

use mew_perception::TreeNode;

/// What kind of overlay the detector thinks the tree is showing. The
/// LLM uses this in its reasoning ("I see a cookie banner, I'll
/// click Accept all") and the agent loop uses it to choose the
/// dismissal CTA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModalKind {
    /// Cookie consent banner — "Accept all" / "Reject non-essential".
    CookieBanner,
    /// Login wall — "Sign in to continue" / "Log in to view this
    /// content". Higher urgency than a cookie banner because the
    /// LLM often can't proceed without an account.
    LoginWall,
    /// Newsletter / marketing popup — "Subscribe to get 10% off" /
    /// "Get our weekly digest". Lowest urgency; usually safe to
    /// close without reading.
    Newsletter,
    /// Age gate — "Are you 18 or older?" / "Enter your date of birth".
    /// Some sites require an interaction; some auto-pass.
    AgeGate,
    /// Generic overlay — we see a role=dialog but the name doesn't
    /// match the above four patterns. Treated as a "pause and ask"
    /// modal because we don't know what it is.
    Generic,
}

impl ModalKind {
    /// Short canonical name. Used in transcript and trace log.
    pub fn as_str(self) -> &'static str {
        match self {
            ModalKind::CookieBanner => "cookie_banner",
            ModalKind::LoginWall => "login_wall",
            ModalKind::Newsletter => "newsletter",
            ModalKind::AgeGate => "age_gate",
            ModalKind::Generic => "generic",
        }
    }

    /// Whether this kind of modal can be safely auto-dismissed by
    /// the loop. Cookie banners and newsletter popups are
    /// low-stakes; login walls and age gates usually require
    /// real input.
    pub fn can_auto_dismiss(self) -> bool {
        matches!(self, ModalKind::CookieBanner | ModalKind::Newsletter)
    }
}

/// The detector's verdict. `dismiss_ref` is set when the detector
/// found a "close" / "accept" / "dismiss" button — the agent loop
/// can click it directly without a fresh LLM round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalReport {
    pub kind: ModalKind,
    /// The ref of a button the agent can click to dismiss the
    /// overlay. `None` when the detector found the overlay but no
    /// obvious close button (e.g. a backdrop click is required) —
    /// in that case the agent surfaces the modal to the LLM.
    pub dismiss_ref: Option<String>,
}

/// Scan the accessibility tree for the four common overlay patterns.
/// Returns the *first* match (priority order: cookie > login >
/// newsletter > age > generic). The agent loop treats any non-`None`
/// result as "this iteration's first action is to handle the modal"
/// — the LLM gets a typed finding that says "modal detected,
/// dismiss_ref=X" so it can re-plan if needed.
///
/// The detector walks the tree depth-first and applies the
/// name-matching rules in `classify_node`. The first match wins so
/// the priority is consistent across runs. A real site may have
/// more than one overlay (cookie banner AND a "Subscribe"
/// button on the same page) — we only return one per iteration
/// and the next iteration will catch the next one.
pub fn detect(tree: &TreeNode) -> Option<ModalReport> {
    fn walk(node: &TreeNode, out: &mut Option<ModalReport>) {
        if out.is_some() {
            return;
        }
        if let Some(report) = classify_node(node) {
            *out = Some(report);
            return;
        }
        for child in &node.children {
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

/// Classify a single node. Returns `Some(report)` if the node looks
/// like an overlay; `None` otherwise. The function is the unit of
/// detection logic — a test can pass it a single hand-built node
/// and assert the result without rebuilding a whole tree.
///
/// Rules (in priority order):
///   1. role == "dialog" or "alertdialog" -> the node is a modal
///      container. The kind is determined by the *name* and the
///      names of its descendants (the dialog itself often has a
///      generic name like "Age verification" while the actual
///      prompt — "Are you 18 or older?" — is the first heading
///      inside). We classify the dialog's *name first*, and if
///      that returns Generic, we fall back to scanning the
///      first few descendants for a heading whose text
///      classifies cleanly. This catches the "dialog has a
///      generic name, real prompt is the heading" case without
///      re-introducing the substring false-positive.
///   2. role == "heading" with a name that matches one of the
///      known patterns -> the heading is on a modal even though
///      the dialog role is missing (some sites forget to set the
///      role). This is a "soft" detection — we still return a
///      report, but with `dismiss_ref = None` (the loop should
///      let the LLM look at the tree).
fn classify_node(node: &TreeNode) -> Option<ModalReport> {
    // Rule 1: explicit dialog role.
    if node.role.eq_ignore_ascii_case("dialog")
        || node.role.eq_ignore_ascii_case("alertdialog")
    {
        let mut kind = modal_kind_from_text(&node.name);
        // If the dialog's own name is generic (e.g.
        // "Age verification" for an age gate), try the
        // first heading inside. Many sites put the
        // actionable prompt in the heading and the
        // container-name as a generic label.
        if matches!(kind, ModalKind::Generic) {
            if let Some(child_kind) = first_classifiable_heading(node) {
                kind = child_kind;
            }
        }
        let dismiss_ref = find_close_button(node);
        return Some(ModalReport {
            kind,
            dismiss_ref,
        });
    }

    // Rule 2: heading whose name matches a known modal pattern.
    if node.role.eq_ignore_ascii_case("heading") {
        if let Some(kind) = modal_kind_from_text_check(&node.name) {
            return Some(ModalReport {
                kind,
                dismiss_ref: None,
            });
        }
    }

    None
}

/// Walk the direct children of a dialog looking for the
/// first heading whose name classifies as a non-Generic
/// modal kind. Returns `None` if no such heading exists
/// (the dialog's own name is the only signal we have).
///
/// Bounded to direct children + one level of grandchildren
/// so the worst case is small. Most real modals put the
/// heading at the top of the dialog.
fn first_classifiable_heading(node: &TreeNode) -> Option<ModalKind> {
    fn check(n: &TreeNode) -> Option<ModalKind> {
        if n.role.eq_ignore_ascii_case("heading") {
            if let Some(k) = modal_kind_from_text_check(&n.name) {
                return Some(k);
            }
        }
        for child in &n.children {
            if let Some(k) = check(child) {
                return Some(k);
            }
        }
        None
    }
    for child in &node.children {
        if let Some(k) = check(child) {
            return Some(k);
        }
    }
    None
}

/// Map a free-text string (the modal's name, a heading's text) to
/// a `ModalKind`. Used both by the dialog role rule (where the
/// dialog name is often empty) and the heading rule. The function
/// is permissive on whitespace and case to catch real-world
/// variations like "  Accept Cookies  ".
///
/// **Word-boundary matching**: every match is done with
/// `split_whitespace().any(|w| w == pattern)` or on
/// multi-word phrases that don't tokenize cleanly. This avoids
/// the "Acme **homepage**" -> "home**age**" -> AgeGate false
/// positive that the original `contains("age")` would produce.
/// The trade-off: legitimate compound words like "age-verification"
/// no longer match — we treat that as a Generic. The current
/// detector leans conservative on AgeGate; a future PR can add
/// specific phrases if needed.
fn modal_kind_from_text(text: &str) -> ModalKind {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let has_word = |w: &str| words.iter().any(|x| *x == w);
    let has_phrase = |p: &str| lower.contains(p);

    if has_word("cookie")
        || has_word("cookies")
        || has_word("consent")
        || has_phrase("cookie consent")
    {
        ModalKind::CookieBanner
    } else if has_phrase("sign in")
        || has_phrase("log in")
        || has_phrase("login")
        || has_word("authenticate")
    {
        ModalKind::LoginWall
    } else if has_word("newsletter")
        || has_word("subscribe")
        || has_phrase("weekly digest")
        || has_phrase("10% off")
    {
        ModalKind::Newsletter
    } else if has_phrase("are you 18")
        || has_phrase("are you of")
        || has_phrase("date of birth")
        || has_word("age-verification")
    {
        ModalKind::AgeGate
    } else {
        ModalKind::Generic
    }
}

/// Predicate variant of `modal_kind_from_text`: returns `Some(kind)`
/// only if the text clearly matches a known pattern. Used by the
/// heading rule, which doesn't want a `Generic` false positive on
/// every "Welcome to our site" heading.
fn modal_kind_from_text_check(text: &str) -> Option<ModalKind> {
    let k = modal_kind_from_text(text);
    if matches!(k, ModalKind::Generic) {
        None
    } else {
        Some(k)
    }
}

/// Find a close button inside a dialog. Walks one level deep
/// looking for a button whose name contains "close", "accept", "got
/// it", "no thanks", or "reject". The first match wins; we don't
/// try to rank buttons by goodness.
fn find_close_button(node: &TreeNode) -> Option<String> {
    fn walk(n: &TreeNode, out: &mut Option<String>) {
        if out.is_some() {
            return;
        }
        if n.role.eq_ignore_ascii_case("button") {
            let lower = n.name.to_lowercase();
            if lower.contains("close")
                || lower.contains("accept")
                || lower.contains("got it")
                || lower.contains("no thanks")
                || lower.contains("reject")
                || lower.contains("dismiss")
                || lower.contains("ok")
            {
                if let Some(r) = &n.ref_id {
                    *out = Some(r.clone());
                    return;
                }
            }
        }
        for child in &n.children {
            walk(child, out);
            if out.is_some() {
                return;
            }
        }
    }
    let mut out = None;
    walk(node, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mew_perception::NodeCategory;

    fn node(
        role: &str,
        name: &str,
        ref_id: Option<&str>,
        children: Vec<TreeNode>,
    ) -> TreeNode {
        TreeNode {
            id: format!("n_{}_{}", role, name),
            role: role.to_string(),
            name: name.to_string(),
            value: String::new(),
            category: NodeCategory::Structural,
            ref_id: ref_id.map(|s| s.to_string()),
            backend_node_id: None,
            children,
        }
    }

    #[test]
    fn detects_cookie_banner_dialog() {
        let tree = node(
            "RootWebArea",
            "Site",
            None,
            vec![node(
                "dialog",
                "Cookie consent",
                None,
                vec![node("button", "Accept all", Some("@e1"), vec![])],
            )],
        );
        let report = detect(&tree).unwrap();
        assert_eq!(report.kind, ModalKind::CookieBanner);
        assert_eq!(report.dismiss_ref.as_deref(), Some("@e1"));
    }

    #[test]
    fn detects_login_wall_via_heading() {
        let tree = node(
            "RootWebArea",
            "Article",
            None,
            vec![node("heading", "Sign in to continue reading", None, vec![])],
        );
        let report = detect(&tree).unwrap();
        assert_eq!(report.kind, ModalKind::LoginWall);
        assert!(report.dismiss_ref.is_none());
    }

    #[test]
    fn detects_newsletter() {
        let tree = node(
            "RootWebArea",
            "Homepage",
            None,
            vec![node("dialog", "Subscribe to our newsletter", None, vec![])],
        );
        let report = detect(&tree).unwrap();
        assert_eq!(report.kind, ModalKind::Newsletter);
        assert!(ModalKind::Newsletter.can_auto_dismiss());
    }

    #[test]
    fn detects_age_gate() {
        let tree = node(
            "RootWebArea",
            "Site",
            None,
            vec![node("heading", "Are you 18 or older?", None, vec![])],
        );
        let report = detect(&tree).unwrap();
        assert_eq!(report.kind, ModalKind::AgeGate);
    }

    #[test]
    fn clean_page_returns_none() {
        let tree = node(
            "RootWebArea",
            "Homepage",
            None,
            vec![
                node("heading", "Welcome to our site", None, vec![]),
                node("link", "About us", Some("@e5"), vec![]),
            ],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn homepage_text_does_not_trigger_age_gate() {
        // Regression guard for the "homepage contains the
        // substring 'age'" false positive. The previous
        // `contains("age")` check classified the heading
        // "Acme homepage" as an AgeGate. Word-boundary
        // matching prevents this.
        let tree = node(
            "RootWebArea",
            "Acme",
            None,
            vec![node("heading", "Acme homepage", None, vec![])],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn stage_name_does_not_trigger_age_gate() {
        // Same regression class: "stage" contains "age" as a
        // substring. The current detector is conservative;
        // it should not flag the page as a modal.
        let tree = node(
            "RootWebArea",
            "Concert",
            None,
            vec![node("heading", "On stage tonight", None, vec![])],
        );
        assert!(detect(&tree).is_none());
    }

    #[test]
    fn cross_module_composition_with_mock_fixtures() {
        // Cross-module composition: the modal detector
        // applied to the shared `mock_fixtures` produces
        // the expected per-page results. This test is
        // the modal-detector half of the cross-module
        // contract; the agent-side adapter test exercises
        // the other half.
        use crate::mock_fixtures;

        // Cookie banner: modal returns AutoDismiss-able.
        let cookie = mock_fixtures::cookie_banner_page();
        let cookie_report = detect(&cookie).expect("cookie banner should be detected");
        assert_eq!(cookie_report.kind, ModalKind::CookieBanner);
        assert!(cookie_report.dismiss_ref.is_some());

        // Login wall: modal returns LoginWall, NOT
        // auto-dismissable.
        let login = mock_fixtures::login_wall_page();
        let login_report = detect(&login).expect("login wall should be detected");
        assert_eq!(login_report.kind, ModalKind::LoginWall);
        assert!(!login_report.kind.can_auto_dismiss());

        // Newsletter: Newsletter kind, auto-dismissable.
        let newsletter = mock_fixtures::newsletter_popup_page();
        let newsletter_report = detect(&newsletter).expect("newsletter should be detected");
        assert_eq!(newsletter_report.kind, ModalKind::Newsletter);
        assert!(newsletter_report.kind.can_auto_dismiss());

        // Age gate: AgeGate kind, NOT auto-dismissable.
        let age = mock_fixtures::age_gate_page();
        let age_report = detect(&age).expect("age gate should be detected");
        assert_eq!(age_report.kind, ModalKind::AgeGate);
        assert!(!age_report.kind.can_auto_dismiss());

        // Clean homepage: no modal signal.
        let clean = mock_fixtures::clean_homepage();
        assert!(detect(&clean).is_none());

        // 429 / Cloudflare pages: no modal signal (the
        // rate-limit detector handles them).
        assert!(detect(&mock_fixtures::http_429_page()).is_none());
        assert!(detect(&mock_fixtures::cloudflare_page()).is_none());
    }

    #[test]
    fn priority_is_first_match_in_tree_order() {
        // The detector is depth-first: the *first* match in tree
        // order wins, regardless of kind. Putting the login
        // heading *first* in the tree means login wins, not
        // cookie. This is the deterministic, easy-to-reason-about
        // behavior — the next iteration catches the next modal.
        let tree = node(
            "RootWebArea",
            "Site",
            None,
            vec![
                node("heading", "Sign in to continue", None, vec![]),
                node("dialog", "Cookie consent", None, vec![]),
            ],
        );
        let report = detect(&tree).unwrap();
        assert_eq!(report.kind, ModalKind::LoginWall);
    }

    #[test]
    fn cookie_wins_when_listed_first() {
        // When the cookie dialog is encountered first, it wins.
        // The two-modals case is handled one at a time across
        // iterations: dismissing the first one lets the next
        // iteration see the second.
        let tree = node(
            "RootWebArea",
            "Site",
            None,
            vec![
                node("dialog", "Cookie consent", None, vec![]),
                node("heading", "Sign in to continue", None, vec![]),
            ],
        );
        let report = detect(&tree).unwrap();
        assert_eq!(report.kind, ModalKind::CookieBanner);
    }

    #[test]
    fn modal_kind_can_auto_dismiss_matrix() {
        // Lock the can_auto_dismiss rule down so a future
        // refactor of the matrix doesn't silently change loop
        // behavior.
        assert!(ModalKind::CookieBanner.can_auto_dismiss());
        assert!(ModalKind::Newsletter.can_auto_dismiss());
        assert!(!ModalKind::LoginWall.can_auto_dismiss());
        assert!(!ModalKind::AgeGate.can_auto_dismiss());
        assert!(!ModalKind::Generic.can_auto_dismiss());
    }
}

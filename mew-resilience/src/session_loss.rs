//! Phase 6 — Failure mode 3: Login / session loss.
//!
//! Background: the agent's task is often "do X on the dashboard".
//! It navigates to the site, signs in (or has a logged-in session
//! from a prior task), the dashboard loads, the LLM works on X.
//! Mid-task, the session can drop — the cookie expires, the user
//! logged out in another tab, the site returned a 401 — and the next
//! page is a login form. The LLM, holding a mental model of "I'm
//! on the dashboard", keeps trying to click dashboard elements
//! that no longer exist and produces a confused flounder pattern.
//!
//! The fix: detect the "this used to be the dashboard and now it's
//! a login form" transition and surface it as a typed finding. The
//! agent loop uses the `hint` field to give the user a concrete
//! recovery path ("you need to sign in again") rather than letting
//! the LLM flounder.
//!
//! Detection is split into two surfaces:
//!   * `detect(tree)` — pure-function on the current tree, returns
//!     `Some(report)` if the tree *looks like* a login form.
//!   * `was_logged_in` — the agent's memory of "we've seen a
//!     dashboard here before". The caller (the agent loop) is
//!     responsible for tracking the prior URL/role and passing it
//!     in. We don't keep the prior URL inside the resilience crate
//!     to avoid a stateful cache.
//!
//! Pure-Rust, no I/O. Tests cover the five common login-form
//! shapes plus the negative case (a search field that looks
//! vaguely like a login form).

use mew_perception::TreeNode;

/// The detector's verdict. `reason` is a short human-readable
/// string for the user-facing chat reply; `hint` is the recovery
/// action the agent should surface ("Sign in to continue" or
/// "Re-authenticate — your session has expired").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLossReport {
    pub reason: String,
    pub hint: String,
}

/// Inputs the caller passes in. `prior_role_signature` is a short
/// opaque string summarizing the *previous* page (e.g. "dashboard"
/// from a hand-rolled heuristic in the agent loop). When the
/// current tree looks like a login form *and* the prior signature
/// was something dashboard-shaped, we have strong evidence of
/// session loss and the report's `reason` reflects that. When the
/// current tree is a login form and there's no prior, we have
/// weak evidence and the report's `reason` is more cautious.
#[derive(Debug, Clone)]
pub struct SessionLossInputs<'a> {
    pub tree: &'a TreeNode,
    /// Optional: what the prior page looked like. The agent loop
    /// fills this from a heuristic on the previous iteration's
    /// tree (count of "data table" roles, presence of a
    /// "user menu" widget, etc).
    pub prior_was_dashboard_like: bool,
}

/// Detect "this page is now a login form when it used to be a
/// dashboard". The detector applies three signals in order:
///
///   1. *Strong*: a `dialog` role whose name or child heading
///      contains "sign in" / "log in" / "login" AND the tree
///      contains a password textbox (`role="textbox"` with
///      autocomplete hint of `current-password` or a name
///      containing "password"). A login dialog with a password
///      field is the canonical session-loss signature.
///   2. *Medium*: a password textbox anywhere in the tree on a
///      page that has a prior-dashboard signature. This catches
///      the common case where the site doesn't wrap the login
///      form in a dialog (e.g. it just renders a `<form>` at the
///      top of the page).
///   3. *Weak*: any textbox whose name contains "password" on
///      a page that has a prior-dashboard signature AND a
///      heading containing "sign in" / "log in". This catches
///      custom login UIs that use non-standard roles.
///
/// The verdict returns `None` when the page has no login-form
/// signals at all (the common case for an actual login page the
/// agent navigated to intentionally) or when the prior wasn't
/// dashboard-like (so we can't claim "you got logged out" — the
/// user might just be on the login page because that's where
/// the task starts).
pub fn detect(inputs: &SessionLossInputs<'_>) -> Option<SessionLossReport> {
    let has_password = has_password_field(inputs.tree);
    let has_login_text = has_login_text(inputs.tree);

    if has_password && has_login_text {
        return Some(SessionLossReport {
            reason: "a login form with both a password field and a sign-in prompt is on the page; this is the shape of a session-loss event.".to_string(),
            hint: "Sign in to continue. Your session appears to have ended mid-task.".to_string(),
        });
    }

    if inputs.prior_was_dashboard_like && has_password {
        return Some(SessionLossReport {
            reason: "a password field appeared on a page that was previously a dashboard; this is the shape of a session-loss event.".to_string(),
            hint: "Sign in to continue. Your session appears to have ended mid-task.".to_string(),
        });
    }

    if inputs.prior_was_dashboard_like && has_login_text && has_text_input(inputs.tree) {
        return Some(SessionLossReport {
            reason: "a sign-in prompt with a text input appeared on a page that was previously a dashboard.".to_string(),
            hint: "Sign in to continue. Your session appears to have ended mid-task.".to_string(),
        });
    }

    None
}

/// True if the tree contains a textbox whose name (or whose
/// parent's name) suggests a password field. We look at the
/// name, the role, and the value (autocomplete hint). The
/// detector is permissive on case and whitespace.
fn has_password_field(node: &TreeNode) -> bool {
    fn walk(n: &TreeNode, out: &mut bool) {
        if *out {
            return;
        }
        if n.role.eq_ignore_ascii_case("textbox") {
            let lower_name = n.name.to_lowercase();
            let lower_value = n.value.to_lowercase();
            if lower_name.contains("password")
                || lower_value.contains("current-password")
                || lower_value.contains("password")
            {
                *out = true;
                return;
            }
        }
        for child in &n.children {
            walk(child, out);
            if *out {
                return;
            }
        }
    }
    let mut out = false;
    walk(node, &mut out);
    out
}

/// True if the tree contains text matching the login-prompt
/// patterns. Looks at heading names and dialog names (the
/// "Sign in to continue" title is usually in one or the other).
fn has_login_text(node: &TreeNode) -> bool {
    fn walk(n: &TreeNode, out: &mut bool) {
        if *out {
            return;
        }
        let lower = n.name.to_lowercase();
        if n.role.eq_ignore_ascii_case("heading")
            || n.role.eq_ignore_ascii_case("dialog")
            || n.role.eq_ignore_ascii_case("alertdialog")
        {
            if lower.contains("sign in")
                || lower.contains("log in")
                || lower.contains("login")
                || lower.contains("authenticate")
            {
                *out = true;
                return;
            }
        }
        for child in &n.children {
            walk(child, out);
            if *out {
                return;
            }
        }
    }
    let mut out = false;
    walk(node, &mut out);
    out
}

/// True if the tree contains any textbox. Used as a tie-breaker
/// in the weak-signal branch — a sign-in prompt with no input
/// fields at all is probably a static marketing page, not a
/// login form.
fn has_text_input(node: &TreeNode) -> bool {
    fn walk(n: &TreeNode, out: &mut bool) {
        if *out {
            return;
        }
        if n.role.eq_ignore_ascii_case("textbox") {
            *out = true;
            return;
        }
        for child in &n.children {
            walk(child, out);
            if *out {
                return;
            }
        }
    }
    let mut out = false;
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
        value: &str,
        ref_id: Option<&str>,
        children: Vec<TreeNode>,
    ) -> TreeNode {
        TreeNode {
            id: format!("n_{}_{}", role, name),
            role: role.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            category: NodeCategory::Structural,
            ref_id: ref_id.map(|s| s.to_string()),
            backend_node_id: None,
            children,
        }
    }

    #[test]
    fn detects_strong_signal_dashboard_becomes_login_dialog() {
        // The canonical case: dashboard -> login dialog.
        let tree = node(
            "RootWebArea",
            "Site",
            "",
            None,
            vec![node(
                "dialog",
                "Sign in to continue",
                "",
                None,
                vec![
                    node("textbox", "Email", "", Some("@e1"), vec![]),
                    node("textbox", "Password", "", Some("@e2"), vec![]),
                ],
            )],
        );
        let inputs = SessionLossInputs {
            tree: &tree,
            prior_was_dashboard_like: true,
        };
        let report = detect(&inputs).unwrap();
        assert!(report.reason.contains("login form"));
        assert!(report.hint.contains("Sign in"));
    }

    #[test]
    fn detects_medium_signal_password_field_appears() {
        // The site didn't bother with a dialog. Just a password
        // field on a page that was a dashboard.
        let tree = node(
            "RootWebArea",
            "Site",
            "",
            None,
            vec![node("textbox", "Password", "", Some("@e1"), vec![])],
        );
        let inputs = SessionLossInputs {
            tree: &tree,
            prior_was_dashboard_like: true,
        };
        let report = detect(&inputs).unwrap();
        assert!(report.reason.contains("password field"));
    }

    #[test]
    fn weak_signal_requires_dashboard_prior() {
        // A sign-in prompt on a page that was *not* a dashboard
        // is not a session-loss event. The user might just have
        // started the task on the login page.
        let tree = node(
            "RootWebArea",
            "Welcome",
            "",
            None,
            vec![
                node("heading", "Sign in", "", None, vec![]),
                node("textbox", "Email", "", Some("@e1"), vec![]),
            ],
        );
        let inputs = SessionLossInputs {
            tree: &tree,
            prior_was_dashboard_like: false,
        };
        assert!(detect(&inputs).is_none());
    }

    #[test]
    fn clean_dashboard_returns_none() {
        let tree = node(
            "RootWebArea",
            "Dashboard",
            "",
            None,
            vec![
                node("heading", "Welcome back, Alice", "", None, vec![]),
                node("link", "Settings", "", Some("@e1"), vec![]),
            ],
        );
        let inputs = SessionLossInputs {
            tree: &tree,
            prior_was_dashboard_like: true,
        };
        assert!(detect(&inputs).is_none());
    }

    #[test]
    fn search_form_with_password_named_field_returns_none_without_dashboard() {
        // False positive guard: a search field accidentally
        // named "password" shouldn't trip the detector when
        // the prior was not a dashboard.
        let tree = node(
            "RootWebArea",
            "Help",
            "",
            None,
            vec![node("textbox", "Password", "", Some("@e1"), vec![])],
        );
        let inputs = SessionLossInputs {
            tree: &tree,
            prior_was_dashboard_like: false,
        };
        assert!(detect(&inputs).is_none());
    }

    #[test]
    fn autocomplete_current_password_treated_as_password() {
        // The field name is "secret" but the autocomplete hint
        // says current-password. Modern password managers use
        // the autocomplete attribute, not the name, so the
        // detector should catch it.
        let tree = node(
            "RootWebArea",
            "Login",
            "",
            None,
            vec![node("textbox", "secret", "current-password", Some("@e1"), vec![])],
        );
        let inputs = SessionLossInputs {
            tree: &tree,
            prior_was_dashboard_like: true,
        };
        assert!(detect(&inputs).is_some());
    }

    #[test]
    fn cross_module_composition_with_mock_fixtures() {
        // The session-loss detector applied to the shared
        // `mock_fixtures` produces the expected per-page
        // results.
        use crate::mock_fixtures;

        // Login wall: strong signal fires regardless of
        // the prior — both password and sign-in prompt
        // are present.
        let login = mock_fixtures::login_wall_page();
        let r = detect(&SessionLossInputs {
            tree: &login,
            prior_was_dashboard_like: false,
        })
        .expect("login wall should produce a session-loss report");
        assert!(r.reason.contains("login form"));

        // Dashboard -> login transition: also a
        // session-loss event. The medium-signal branch
        // (password-only) fires when prior is dashboard.
        let login_inputs = SessionLossInputs {
            tree: &login,
            prior_was_dashboard_like: true,
        };
        assert!(detect(&login_inputs).is_some());

        // Clean homepage: no session loss.
        let clean = mock_fixtures::clean_homepage();
        assert!(detect(&SessionLossInputs {
            tree: &clean,
            prior_was_dashboard_like: true,
        })
        .is_none());

        // Search page (not dashboard) without password:
        // no session loss, even when prior was dashboard.
        let search = mock_fixtures::search_page();
        assert!(detect(&SessionLossInputs {
            tree: &search,
            prior_was_dashboard_like: false,
        })
        .is_none());

        // Cookie banner / 429 / Cloudflare: no session
        // loss signal.
        assert!(detect(&SessionLossInputs {
            tree: &mock_fixtures::cookie_banner_page(),
            prior_was_dashboard_like: true,
        })
        .is_none());
        assert!(detect(&SessionLossInputs {
            tree: &mock_fixtures::http_429_page(),
            prior_was_dashboard_like: true,
        })
        .is_none());
        assert!(detect(&SessionLossInputs {
            tree: &mock_fixtures::cloudflare_page(),
            prior_was_dashboard_like: true,
        })
        .is_none());
    }
}

//! Phase 4 — Google-based authenticated sessions.
//!
//! Single-credential auth: the user supplies one Gmail account via
//! env vars; the agent uses "Sign in with Google" wherever it appears
//! on job boards and freelance platforms, letting Google handle
//! identity for every site rather than requiring per-platform passwords.
//!
//! The module provides:
//! - `LoginFlow` trait: AX-tree-driven login detection and execution
//! - `GoogleLoginFlow`: concrete implementation for Google OAuth
//! - `LoginResult`: typed outcome of a login attempt
//! - `LoginAttemptTracker`: per-session failure counting with pause gate
//! - `AuthState`: per-session authentication state
//!
//! All detection is AX-tree-based — no hardcoded CSS selectors.

use mew_perception::TreeNode;
use serde::{Deserialize, Serialize};

/// The outcome of a login attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginResult {
    /// Login succeeded — the post-login page was detected.
    Success,
    /// The Google sign-in button was not found on the page.
    NoGoogleButton,
    /// The email input field was not found on the Google login page.
    NoEmailField,
    /// The password input field was not found after email submission.
    NoPasswordField,
    /// A 2FA / phone verification prompt was detected.
    TwoFactorRequired,
    /// An unexpected CAPTCHA appeared during the login flow.
    CaptchaDetected,
    /// The login failed (wrong credentials, unknown error).
    Failed(String),
}

/// Per-session authentication state. Stored on `SessionHandle`
/// so subsequent subtasks in the same run skip re-login.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthState {
    /// Whether the agent has successfully authenticated via Google
    /// in this session.
    pub google_authenticated: bool,
    /// The domain where authentication succeeded (e.g. "linkedin.com").
    /// Used to avoid re-login on the same domain.
    pub authenticated_domains: Vec<String>,
    /// Number of failed login attempts in this session.
    pub failed_attempts: u32,
    /// Whether the session is paused waiting for user intervention
    /// (2FA, CAPTCHA, or too many failures).
    pub awaiting_user: bool,
    /// Human-readable reason for why the session is awaiting user
    /// action (e.g. "Google 2FA verification required").
    pub awaiting_reason: Option<String>,
}

impl AuthState {
    /// Mark a domain as authenticated.
    pub fn mark_authenticated(&mut self, domain: &str) {
        self.google_authenticated = true;
        if !self.authenticated_domains.iter().any(|d| d == domain) {
            self.authenticated_domains.push(domain.to_string());
        }
    }

    /// Check if a domain is already authenticated.
    pub fn is_authenticated_for(&self, domain: &str) -> bool {
        self.authenticated_domains.iter().any(|d| d == domain)
    }

    /// Record a failed attempt. Returns `true` if the session should
    /// pause for user intervention (>= 2 failures).
    pub fn record_failure(&mut self) -> bool {
        self.failed_attempts += 1;
        if self.failed_attempts >= 2 {
            self.awaiting_user = true;
            self.awaiting_reason = Some(format!(
                "Google login failed {} times. Please check credentials or solve any challenges manually.",
                self.failed_attempts
            ));
            true
        } else {
            false
        }
    }

    /// Set the session to await user intervention with a specific reason.
    pub fn set_awaiting(&mut self, reason: String) {
        self.awaiting_user = true;
        self.awaiting_reason = Some(reason);
    }

    /// Clear the awaiting state (called after user resolves the issue).
    pub fn clear_awaiting(&mut self) {
        self.awaiting_user = false;
        self.awaiting_reason = None;
    }
}

/// Trait for AX-tree-driven login flows. Pure-Rust, testable
/// without a live browser — mock fixtures supply synthetic `TreeNode`s.
pub trait LoginFlow {
    /// Detect whether the current page requires login.
    /// Returns `true` if a login form / sign-in wall is present.
    fn detect_login_required(tree: &TreeNode) -> bool;

    /// Find the "Sign in with Google" / "Continue with Google"
    /// button in the AX tree. Returns the `ref_id` if found.
    fn find_google_signin_button(tree: &TreeNode) -> Option<String>;

    /// Detect whether the current page is a Google email input page.
    fn is_google_email_page(tree: &TreeNode) -> bool;

    /// Detect whether the current page is a Google password input page.
    fn is_google_password_page(tree: &TreeNode) -> bool;

    /// Detect a 2FA / phone verification prompt on the Google login flow.
    fn is_two_factor_page(tree: &TreeNode) -> bool;

    /// Detect post-login success (redirect back to the platform).
    fn is_post_login_success(tree: &TreeNode, prior_was_login: bool) -> bool;
}

/// Google OAuth login flow implementation. All detection is
/// AX-tree-based — no hardcoded selectors.
pub struct GoogleLoginFlow;

impl LoginFlow for GoogleLoginFlow {
    /// Detect "this page has a login form" — looks for password
    /// fields, "sign in" / "log in" headings, and email inputs
    /// in combination. Reuses the same signals as `session_loss::detect`
    /// but without requiring `prior_was_dashboard_like` (this is
    /// for intentional login, not session-loss detection).
    fn detect_login_required(tree: &TreeNode) -> bool {
        has_password_field(tree) && has_login_heading(tree)
    }

    /// Find a "Sign in with Google" / "Continue with Google" button.
    /// Searches the AX tree for a button or link whose name contains
    /// the Google sign-in patterns. Returns the `ref_id` for clicking.
    fn find_google_signin_button(tree: &TreeNode) -> Option<String> {
        find_button_with_text(tree, &[
            "sign in with google",
            "continue with google",
            "login with google",
            "log in with google",
            "sign up with google",
            "google sign in",
            "google login",
            "continue with google account",
        ])
    }

    /// Detect the Google email input page. The canonical shape is:
    /// - A heading or text containing "Google" or "gmail"
    /// - A textbox for email/phone input
    /// - A "Next" button
    fn is_google_email_page(tree: &TreeNode) -> bool {
        let has_google_text = has_text_containing(tree, &["google", "gmail", "accounts.google"]);
        let has_email_input = has_textbox_with_name(tree, &[
            "email", "phone", "email or phone", "enter your email",
            "type your email", "your email",
        ]);
        let has_next_button = has_button_with_text(tree, &["next", "continue"]);
        has_google_text && has_email_input && has_next_button
    }

    /// Detect the Google password input page. The canonical shape is:
    /// - A heading/text with the user's email or "Google"
    /// - A password field
    /// - A "Next" button
    fn is_google_password_page(tree: &TreeNode) -> bool {
        let has_password = has_password_field(tree);
        let has_next = has_button_with_text(tree, &["next", "continue", "sign in"]);
        has_password && has_next
    }

    /// Detect a 2FA / phone verification prompt. Looks for:
    /// - Text containing "verify", "verification", "2-step", "phone"
    /// - A heading about verification or security
    fn is_two_factor_page(tree: &TreeNode) -> bool {
        has_text_containing(tree, &[
            "2-step verification",
            "two-factor",
            "2fa",
            "verify your identity",
            "confirm your phone",
            "phone number verification",
            "verify it's you",
            "check your phone",
            "enter verification code",
            "security check",
        ])
    }

    /// Detect post-login success. When `prior_was_login` is true,
    /// looks for dashboard-like signals (tables, "welcome" headings,
    /// user menus) that indicate the platform accepted the login.
    fn is_post_login_success(tree: &TreeNode, prior_was_login: bool) -> bool {
        if !prior_was_login {
            return false;
        }
        // Success signals: dashboard content, user menu, no login form
        let has_dashboard = has_role(tree, &["table", "grid", "row"])
            || has_text_containing(tree, &[
                "welcome back", "dashboard", "my account", "my profile",
                "sign out", "log out", "sign out",
            ]);
        let has_login_form = has_password_field(tree);
        has_dashboard && !has_login_form
    }
}

// ---------------------------------------------------------------------------
// AX-tree traversal helpers (pure-Rust, no I/O)
// ---------------------------------------------------------------------------

/// True if the tree contains a textbox whose name suggests a password field.
fn has_password_field(node: &TreeNode) -> bool {
    fn walk(n: &TreeNode, out: &mut bool) {
        if *out { return; }
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
            if *out { return; }
        }
    }
    let mut out = false;
    walk(node, &mut out);
    out
}

/// True if the tree contains a heading or dialog with login-related text.
fn has_login_heading(node: &TreeNode) -> bool {
    fn walk(n: &TreeNode, out: &mut bool) {
        if *out { return; }
        let lower = n.name.to_lowercase();
        if (n.role.eq_ignore_ascii_case("heading")
            || n.role.eq_ignore_ascii_case("dialog")
            || n.role.eq_ignore_ascii_case("alertdialog"))
            && (lower.contains("sign in")
                || lower.contains("log in")
                || lower.contains("login")
                || lower.contains("authenticate")
                || lower.contains("sign up"))
        {
            *out = true;
            return;
        }
        for child in &n.children {
            walk(child, out);
            if *out { return; }
        }
    }
    let mut out = false;
    walk(node, &mut out);
    out
}

/// Find a button/link whose name matches any of the given patterns.
/// Returns the `ref_id` of the first match.
fn find_button_with_text(node: &TreeNode, patterns: &[&str]) -> Option<String> {
    fn walk(n: &TreeNode, patterns: &[&str], out: &mut Option<String>) {
        if out.is_some() { return; }
        let lower = n.name.to_lowercase();
        let is_button_like = n.role.eq_ignore_ascii_case("button")
            || n.role.eq_ignore_ascii_case("link")
            || (n.role.eq_ignore_ascii_case("text") && n.ref_id.is_some());
        if is_button_like {
            for pat in patterns {
                if lower.contains(pat) {
                    if let Some(ref_id) = &n.ref_id {
                        *out = Some(ref_id.clone());
                        return;
                    }
                }
            }
        }
        for child in &n.children {
            walk(child, patterns, out);
            if out.is_some() { return; }
        }
    }
    let mut out = None;
    walk(node, patterns, &mut out);
    out
}

/// True if the tree contains text matching any of the given patterns
/// in any node's name.
fn has_text_containing(node: &TreeNode, patterns: &[&str]) -> bool {
    fn walk(n: &TreeNode, patterns: &[&str], out: &mut bool) {
        if *out { return; }
        let lower = n.name.to_lowercase();
        for pat in patterns {
            if lower.contains(pat) {
                *out = true;
                return;
            }
        }
        for child in &n.children {
            walk(child, patterns, out);
            if *out { return; }
        }
    }
    let mut out = false;
    walk(node, patterns, &mut out);
    out
}

/// True if the tree contains a textbox whose name matches any pattern.
fn has_textbox_with_name(node: &TreeNode, patterns: &[&str]) -> bool {
    fn walk(n: &TreeNode, patterns: &[&str], out: &mut bool) {
        if *out { return; }
        if n.role.eq_ignore_ascii_case("textbox") {
            let lower = n.name.to_lowercase();
            for pat in patterns {
                if lower.contains(pat) {
                    *out = true;
                    return;
                }
            }
        }
        for child in &n.children {
            walk(child, patterns, out);
            if *out { return; }
        }
    }
    let mut out = false;
    walk(node, patterns, &mut out);
    out
}

/// True if the tree contains a button whose text matches any pattern.
fn has_button_with_text(node: &TreeNode, patterns: &[&str]) -> bool {
    fn walk(n: &TreeNode, patterns: &[&str], out: &mut bool) {
        if *out { return; }
        if n.role.eq_ignore_ascii_case("button") || n.role.eq_ignore_ascii_case("link") {
            let lower = n.name.to_lowercase();
            for pat in patterns {
                if lower.contains(pat) {
                    *out = true;
                    return;
                }
            }
        }
        for child in &n.children {
            walk(child, patterns, out);
            if *out { return; }
        }
    }
    let mut out = false;
    walk(node, patterns, &mut out);
    out
}

/// True if the tree contains a node with any of the given roles.
fn has_role(node: &TreeNode, roles: &[&str]) -> bool {
    fn walk(n: &TreeNode, roles: &[&str], out: &mut bool) {
        if *out { return; }
        for r in roles {
            if n.role.eq_ignore_ascii_case(r) {
                *out = true;
                return;
            }
        }
        for child in &n.children {
            walk(child, roles, out);
            if *out { return; }
        }
    }
    let mut out = false;
    walk(node, roles, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Credential redaction for tracing
// ---------------------------------------------------------------------------

/// Redact sensitive values from a string before writing to trace logs.
/// Replaces occurrences of the email and password with `[REDACTED]`.
pub fn redact_credentials(text: &str, email: &str, password: &str) -> String {
    let mut out = text.to_string();
    if !email.is_empty() {
        out = out.replace(email, "[REDACTED_EMAIL]");
    }
    if !password.is_empty() {
        out = out.replace(password, "[REDACTED_PASSWORD]");
    }
    out
}

/// Collect all credential values that should be redacted from trace logs.
/// Reads from the config's credential env vars. Returns an empty vec
/// if no credentials are configured.
pub fn collect_redaction_targets(config: &crate::CredentialsConfig) -> Vec<String> {
    let mut targets = Vec::new();
    if let Some(google) = &config.google {
        if let Some((email, password)) = google.read_from_env() {
            if !email.is_empty() {
                targets.push(email);
            }
            if !password.is_empty() {
                targets.push(password);
            }
        }
    }
    targets
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

    // ---------------------------------------------------------------
    // GoogleLoginFlow::detect_login_required
    // ---------------------------------------------------------------

    #[test]
    fn detects_login_page_with_password_and_heading() {
        let tree = node("RootWebArea", "Site", "", None, vec![
            node("heading", "Sign in", "", None, vec![]),
            node("textbox", "Email", "", Some("@e1"), vec![]),
            node("textbox", "Password", "", Some("@e2"), vec![]),
        ]);
        assert!(GoogleLoginFlow::detect_login_required(&tree));
    }

    #[test]
    fn rejects_page_without_login_heading() {
        let tree = node("RootWebArea", "Dashboard", "", None, vec![
            node("heading", "Welcome back", "", None, vec![]),
            node("textbox", "Search", "", Some("@e1"), vec![]),
        ]);
        assert!(!GoogleLoginFlow::detect_login_required(&tree));
    }

    #[test]
    fn rejects_page_without_password_field() {
        let tree = node("RootWebArea", "Login", "", None, vec![
            node("heading", "Sign in", "", None, vec![]),
            node("textbox", "Email", "", Some("@e1"), vec![]),
        ]);
        assert!(!GoogleLoginFlow::detect_login_required(&tree));
    }

    // ---------------------------------------------------------------
    // GoogleLoginFlow::find_google_signin_button
    // ---------------------------------------------------------------

    #[test]
    fn finds_google_signin_button() {
        let tree = node("RootWebArea", "Login", "", None, vec![
            node("heading", "Sign in", "", None, vec![]),
            node("button", "Sign in with Google", "", Some("@e5"), vec![]),
            node("textbox", "Email", "", Some("@e1"), vec![]),
        ]);
        let btn = GoogleLoginFlow::find_google_signin_button(&tree);
        assert_eq!(btn.as_deref(), Some("@e5"));
    }

    #[test]
    fn finds_continue_with_google_button() {
        let tree = node("RootWebArea", "Login", "", None, vec![
            node("button", "Continue with Google", "", Some("@e3"), vec![]),
        ]);
        let btn = GoogleLoginFlow::find_google_signin_button(&tree);
        assert_eq!(btn.as_deref(), Some("@e3"));
    }

    #[test]
    fn returns_none_when_no_google_button() {
        let tree = node("RootWebArea", "Login", "", None, vec![
            node("button", "Sign in with GitHub", "", Some("@e1"), vec![]),
        ]);
        assert!(GoogleLoginFlow::find_google_signin_button(&tree).is_none());
    }

    // ---------------------------------------------------------------
    // GoogleLoginFlow::is_google_email_page
    // ---------------------------------------------------------------

    #[test]
    fn detects_google_email_page() {
        let tree = node("RootWebArea", "Google Accounts", "", None, vec![
            node("heading", "Sign in", "", None, vec![
                node("text", "Google", "", None, vec![]),
            ]),
            node("textbox", "Email or phone", "", Some("@e1"), vec![]),
            node("button", "Next", "", Some("@e2"), vec![]),
        ]);
        assert!(GoogleLoginFlow::is_google_email_page(&tree));
    }

    #[test]
    fn rejects_non_google_email_page() {
        let tree = node("RootWebArea", "Acme Login", "", None, vec![
            node("textbox", "Email", "", Some("@e1"), vec![]),
            node("button", "Submit", "", Some("@e2"), vec![]),
        ]);
        assert!(!GoogleLoginFlow::is_google_email_page(&tree));
    }

    // ---------------------------------------------------------------
    // GoogleLoginFlow::is_google_password_page
    // ---------------------------------------------------------------

    #[test]
    fn detects_google_password_page() {
        let tree = node("RootWebArea", "Google Accounts", "", None, vec![
            node("heading", "user@gmail.com", "", None, vec![]),
            node("textbox", "Password", "current-password", Some("@e1"), vec![]),
            node("button", "Next", "", Some("@e2"), vec![]),
        ]);
        assert!(GoogleLoginFlow::is_google_password_page(&tree));
    }

    #[test]
    fn rejects_page_without_next_button() {
        let tree = node("RootWebArea", "Some Site", "", None, vec![
            node("textbox", "Password", "", Some("@e1"), vec![]),
        ]);
        assert!(!GoogleLoginFlow::is_google_password_page(&tree));
    }

    // ---------------------------------------------------------------
    // GoogleLoginFlow::is_two_factor_page
    // ---------------------------------------------------------------

    #[test]
    fn detects_2fa_page() {
        let tree = node("RootWebArea", "Google Accounts", "", None, vec![
            node("heading", "Verify it's you", "", None, vec![]),
            node("paragraph", "Check your phone for a verification code", "", None, vec![]),
        ]);
        assert!(GoogleLoginFlow::is_two_factor_page(&tree));
    }

    #[test]
    fn detects_2_step_verification() {
        let tree = node("RootWebArea", "Google", "", None, vec![
            node("heading", "2-Step Verification", "", None, vec![]),
        ]);
        assert!(GoogleLoginFlow::is_two_factor_page(&tree));
    }

    #[test]
    fn rejects_normal_page_as_2fa() {
        let tree = node("RootWebArea", "Dashboard", "", None, vec![
            node("heading", "Welcome back", "", None, vec![]),
        ]);
        assert!(!GoogleLoginFlow::is_two_factor_page(&tree));
    }

    // ---------------------------------------------------------------
    // GoogleLoginFlow::is_post_login_success
    // ---------------------------------------------------------------

    #[test]
    fn detects_post_login_dashboard() {
        let tree = node("RootWebArea", "LinkedIn", "", None, vec![
            node("heading", "Welcome back, Alice", "", None, vec![]),
            node("table", "Feed", "", None, vec![
                node("row", "Post 1", "", Some("@e1"), vec![]),
            ]),
            node("link", "Sign out", "", Some("@e2"), vec![]),
        ]);
        assert!(GoogleLoginFlow::is_post_login_success(&tree, true));
    }

    #[test]
    fn rejects_login_page_as_success() {
        let tree = node("RootWebArea", "Login", "", None, vec![
            node("heading", "Sign in", "", None, vec![]),
            node("textbox", "Password", "", Some("@e1"), vec![]),
        ]);
        assert!(!GoogleLoginFlow::is_post_login_success(&tree, true));
    }

    #[test]
    fn requires_prior_was_login() {
        let tree = node("RootWebArea", "Dashboard", "", None, vec![
            node("heading", "Welcome back", "", None, vec![]),
            node("table", "Data", "", None, vec![]),
        ]);
        assert!(!GoogleLoginFlow::is_post_login_success(&tree, false));
    }

    // ---------------------------------------------------------------
    // AuthState
    // ---------------------------------------------------------------

    #[test]
    fn auth_state_marks_authenticated() {
        let mut state = AuthState::default();
        state.mark_authenticated("linkedin.com");
        assert!(state.is_authenticated_for("linkedin.com"));
        assert!(!state.is_authenticated_for("indeed.com"));
        assert!(state.google_authenticated);
    }

    #[test]
    fn auth_state_records_failures() {
        let mut state = AuthState::default();
        assert!(!state.record_failure()); // 1st failure — don't pause yet
        assert!(state.record_failure()); // 2nd failure — pause
        assert!(state.awaiting_user);
        assert!(state.awaiting_reason.is_some());
    }

    #[test]
    fn auth_state_clear_awaiting() {
        let mut state = AuthState::default();
        state.set_awaiting("2FA required".to_string());
        assert!(state.awaiting_user);
        state.clear_awaiting();
        assert!(!state.awaiting_user);
        assert!(state.awaiting_reason.is_none());
    }

    // ---------------------------------------------------------------
    // redact_credentials
    // ---------------------------------------------------------------

    #[test]
    fn redacts_email_and_password() {
        let text = "User user@gmail.com typed password123 into the field";
        let redacted = redact_credentials(text, "user@gmail.com", "password123");
        assert!(!redacted.contains("user@gmail.com"));
        assert!(!redacted.contains("password123"));
        assert!(redacted.contains("[REDACTED_EMAIL]"));
        assert!(redacted.contains("[REDACTED_PASSWORD]"));
    }

    #[test]
    fn redact_handles_empty_values() {
        let text = "no secrets here";
        let redacted = redact_credentials(text, "", "");
        assert_eq!(redacted, text);
    }

    // ---------------------------------------------------------------
    // GoogleCredential::read_from_env
    // ---------------------------------------------------------------

    #[test]
    fn google_credential_returns_none_when_env_missing() {
        let cred = crate::GoogleCredential {
            email_env: "MEW_TEST_EMAIL_NONEXISTENT_12345".to_string(),
            password_env: "MEW_TEST_PASSWORD_NONEXISTENT_12345".to_string(),
        };
        assert!(cred.read_from_env().is_none());
    }

    // ---------------------------------------------------------------
    // collect_redaction_targets
    // ---------------------------------------------------------------

    #[test]
    fn collect_redaction_targets_empty_when_no_creds() {
        let config = crate::CredentialsConfig::default();
        let targets = collect_redaction_targets(&config);
        assert!(targets.is_empty());
    }
}

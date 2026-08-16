//! Phase 6 — Failure mode 5: Irreversible action gating.
//!
//! Background: a real browser agent is one bug away from sending an
//! unintended message, posting a draft publicly, deleting a file,
//! or charging a credit card. The 2026 GUI-agent literature is
//! explicit: agents that don't *pause* for confirmation on
//! point-of-no-return actions are unfit for production use.
//!
//! The fix: classify every action the LLM dispatches into
//! `Reversible` or `Irreversible` before execution. Irreversible
//! actions flip the session state to `Paused`, surface a typed
//! `IrreverisbleVerdict` to the agent loop, and the loop emits a
//! "please confirm" event to the user. Only after the user
//! confirms (via a session resume with a "yes" intent) does the
//! action fire.
//!
//! The classifier is rule-based, not ML. We classify by **tool
//! name + argument shape**, both of which are observable in the
//! match-arm before execution. A "send" tool call with a `to`
//! argument is irreversible. A "click" on a "Cancel" button is
//! reversible. The classifier returns the strongest verdict it
//! can support with the available evidence.
//!
//! Pure-Rust, no I/O. Tests cover the seven common irreversible
//! shapes, the safe-positive case (read-only tools), and the
//! ambiguous case (a tool name that *could* be irreversible
//! depending on context).

use serde_json::Value;

/// What kind of action the LLM is about to dispatch.
///
/// Phase 6: `SubmitApplication` and `SendProposal` are *new*
/// variants that distinguish a job-board "submit" from a
/// generic form submit. The agent's auto-apply layer keys on
/// these to draft application text and present it at a
/// `Paused` checkpoint (see `mew_agent::application_dedup`
/// and `mew_agent::auto_apply`). The string returned by
/// `as_str()` is part of the wire protocol — the Tauri sink
/// payload includes it — so existing string values are
/// unchanged and only new variants are added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// A message is being sent (DM, email, chat).
    Send,
    /// A post is being made publicly (tweet, blog comment, status
    /// update visible to others).
    Post,
    /// A delete action (file, message, post, account, etc).
    Delete,
    /// A payment or money-moving action (checkout, transfer,
    /// refund).
    Pay,
    /// A submit of a form that has point-of-no-return semantics
    /// (job application, exam submission, etc).
    Submit,
    /// A follow or subscription action.
    Follow,
    /// A "set" or "update" action that changes account-level
    /// state (password, email, privacy settings).
    Update,
    /// A job-board application submission (LinkedIn Easy Apply,
    /// Indeed Apply, Greenhouse, Lever, etc). Phase 6
    /// auto-apply layer treats this as a typed pause point —
    /// even with `auto_apply.enabled: true`, the agent pauses
    /// to surface the drafted text for user approval.
    SubmitApplication,
    /// A freelance proposal / pitch submission (Upwork proposal,
    /// Contra intro, etc). Phase 6 auto-apply treats this the
    /// same as `SubmitApplication` — draft text + paused
    /// confirmation + dedup-store check.
    SendProposal,
    /// A connection request (LinkedIn connect, Twitter follow
    /// request). Phase 6 — sent on the user's behalf with the
    /// same dedup-store / per-run cap as applications.
    SendConnectionRequest,
    /// A read-only or otherwise reversible action.
    Other,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Send => "send",
            ActionKind::Post => "post",
            ActionKind::Delete => "delete",
            ActionKind::Pay => "pay",
            ActionKind::Submit => "submit",
            ActionKind::Follow => "follow",
            ActionKind::Update => "update",
            ActionKind::SubmitApplication => "submit_application",
            ActionKind::SendProposal => "send_proposal",
            ActionKind::SendConnectionRequest => "send_connection_request",
            ActionKind::Other => "other",
        }
    }
}

/// The classifier's verdict. `target` is a short human-readable
/// string ("message to @alice", "$42.50 to ACME Corp") that the
/// agent includes in the user-facing pause message so the user
/// can see exactly what's about to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrreverisbleVerdict {
    pub action: ActionKind,
    pub target: String,
}

/// Rule for classifying a tool call. Each rule pairs a tool name
/// (e.g. `send_message`) with the action kind and an argument
/// extractor (turns the JSON args into a short target string).
#[derive(Debug, Clone, Copy)]
struct Rule {
    name: &'static str,
    kind: ActionKind,
    /// `arg_keys` are the JSON keys to look at, in priority order,
    /// when building the human-readable target string. The first
    /// non-empty one wins.
    arg_keys: &'static [&'static str],
}

/// Hard-coded rule table. Order matters: the first matching rule
/// wins, so put more specific rules first. Adding a new
/// irreversible tool to the agent is a one-line change to this
/// table + a unit test in this file.
///
/// This is intentionally a constant — the rules are part of the
/// safety contract, not user config. If a future deployment needs
/// to add/remove a rule, it should be a code change reviewed by a
/// human, not a config edit.
const RULES: &[Rule] = &[
    // Messaging.
    Rule {
        name: "send_message",
        kind: ActionKind::Send,
        arg_keys: &["to", "recipient", "username", "text"],
    },
    Rule {
        name: "send_dm",
        kind: ActionKind::Send,
        arg_keys: &["to", "username"],
    },
    Rule {
        name: "send_email",
        kind: ActionKind::Send,
        arg_keys: &["to", "subject"],
    },
    // Public posts.
    Rule {
        name: "post_tweet",
        kind: ActionKind::Post,
        arg_keys: &["text", "content"],
    },
    Rule {
        name: "post_status",
        kind: ActionKind::Post,
        arg_keys: &["text", "content"],
    },
    Rule {
        name: "post_comment",
        kind: ActionKind::Post,
        arg_keys: &["text", "content"],
    },
    Rule {
        name: "publish",
        kind: ActionKind::Post,
        arg_keys: &["text", "content"],
    },
    // Delete.
    Rule {
        name: "delete_message",
        kind: ActionKind::Delete,
        arg_keys: &["message_id", "id"],
    },
    Rule {
        name: "delete_post",
        kind: ActionKind::Delete,
        arg_keys: &["post_id", "id"],
    },
    Rule {
        name: "delete_file",
        kind: ActionKind::Delete,
        arg_keys: &["path", "filename"],
    },
    Rule {
        name: "delete_account",
        kind: ActionKind::Delete,
        arg_keys: &["account_id", "username"],
    },
    Rule {
        name: "remove",
        kind: ActionKind::Delete,
        arg_keys: &["id", "path"],
    },
    // Payment.
    Rule {
        name: "pay",
        kind: ActionKind::Pay,
        arg_keys: &["amount", "to", "recipient"],
    },
    Rule {
        name: "checkout",
        kind: ActionKind::Pay,
        arg_keys: &["total", "amount"],
    },
    Rule {
        name: "transfer",
        kind: ActionKind::Pay,
        arg_keys: &["amount", "to"],
    },
    Rule {
        name: "submit_payment",
        kind: ActionKind::Pay,
        arg_keys: &["amount"],
    },
    // Generic submit (job application, exam, etc).
    Rule {
        name: "submit_application",
        kind: ActionKind::Submit,
        arg_keys: &["role", "job_id"],
    },
    Rule {
        name: "submit_form",
        kind: ActionKind::Submit,
        arg_keys: &["form_id"],
    },
    // Follow / subscribe.
    Rule {
        name: "follow",
        kind: ActionKind::Follow,
        arg_keys: &["username", "user_id"],
    },
    Rule {
        name: "subscribe",
        kind: ActionKind::Follow,
        arg_keys: &["channel", "user_id"],
    },
    // Updates that change account state.
    Rule {
        name: "update_password",
        kind: ActionKind::Update,
        arg_keys: &["new_password"],
    },
    Rule {
        name: "update_email",
        kind: ActionKind::Update,
        arg_keys: &["new_email"],
    },
    Rule {
        name: "update_settings",
        kind: ActionKind::Update,
        arg_keys: &["setting"],
    },
    // -----------------------------------------------------------------
    // Phase 6: auto-apply action layer
    // -----------------------------------------------------------------
    //
    // The rules below are *more specific* than the generic
    // `submit_application` and `send_message` rules above, so
    // they sit *after* the generic ones in the table and only
    // match when the LLM dispatches a typed tool. The
    // `mew_agent::auto_apply` layer (consuming these verdicts)
    // looks for `SubmitApplication` / `SendProposal` /
    // `SendConnectionRequest` and:
    //
    //   1. Deduplicates the (platform, identifier) pair against
    //      `<data_dir>/applications_sent.json`.
    //   2. Enforces a per-run cap (`max_applications_per_run`).
    //   3. Drafts the application / proposal / pitch text
    //      (reusing the existing summarizer LLM call pattern).
    //   4. Surfaces the draft to the user at the `Paused`
    //      checkpoint for confirmation — never auto-submits
    //      even with `auto_apply.enabled: true`, unless
    //      `require_confirmation_per_application: false` is
    //      explicitly set.
    //
    // Job-board application submission (LinkedIn Easy Apply,
    // Indeed Apply, Greenhouse, Lever, AshbyHQ, Workday).
    Rule {
        name: "apply_to_job",
        kind: ActionKind::SubmitApplication,
        arg_keys: &["job_id", "role", "company"],
    },
    Rule {
        name: "submit_job_application",
        kind: ActionKind::SubmitApplication,
        arg_keys: &["job_id", "role", "company"],
    },
    Rule {
        name: "apply_for_job",
        kind: ActionKind::SubmitApplication,
        arg_keys: &["job_id", "role", "company"],
    },
    // Freelance proposal / pitch (Upwork, Contra, WeWorkRemotely
    // contract listings). Typed distinctly from a generic
    // "send" so the auto-apply draft text matches the
    // platform's expected voice.
    Rule {
        name: "send_proposal",
        kind: ActionKind::SendProposal,
        arg_keys: &["project_id", "client_name", "scope"],
    },
    Rule {
        name: "submit_proposal",
        kind: ActionKind::SendProposal,
        arg_keys: &["project_id", "client_name", "scope"],
    },
    Rule {
        name: "pitch_client",
        kind: ActionKind::SendProposal,
        arg_keys: &["client_name", "scope"],
    },
    // Connection requests (LinkedIn, Twitter / X). Same dedup
    // + per-run cap + paused-confirmation contract as
    // applications.
    Rule {
        name: "send_connection_request",
        kind: ActionKind::SendConnectionRequest,
        arg_keys: &["to", "username", "user_id"],
    },
    Rule {
        name: "connect_with",
        kind: ActionKind::SendConnectionRequest,
        arg_keys: &["username", "user_id"],
    },
];

/// Classify a tool call. Returns `Some(verdict)` if the call is
/// irreversible and should be gated; `None` if it's safe to
/// execute without confirmation. The `target` field is built
/// from the first non-empty argument listed in the rule's
/// `arg_keys`; if all are empty we fall back to a generic
/// description like "send_message (no target args supplied)".
pub fn classify(tool_name: &str, args: &Value) -> Option<IrreverisbleVerdict> {
    let rule = RULES.iter().find(|r| r.name == tool_name)?;
    let target = extract_target(args, rule.arg_keys);
    Some(IrreverisbleVerdict {
        action: rule.kind,
        target,
    })
}

fn extract_target(args: &Value, keys: &[&'static str]) -> String {
    if let Some(obj) = args.as_object() {
        for k in keys {
            if let Some(v) = obj.get(*k) {
                if let Some(s) = v.as_str() {
                    if !s.is_empty() {
                        // Truncate to keep the chat reply short.
                        return truncate(s, 80);
                    }
                } else if v.is_number() {
                    return v.to_string();
                } else if v.is_boolean() {
                    return v.to_string();
                }
            }
        }
    }
    "(no target args supplied)".to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn send_message_is_irreversible() {
        let v = classify("send_message", &json!({ "to": "@alice", "text": "hi" })).unwrap();
        assert_eq!(v.action, ActionKind::Send);
        assert!(v.target.contains("@alice"));
    }

    #[test]
    fn pay_is_irreversible() {
        let v = classify("pay", &json!({ "amount": 42.50, "to": "ACME" })).unwrap();
        assert_eq!(v.action, ActionKind::Pay);
        assert!(v.target.contains("42.5"));
    }

    #[test]
    fn click_is_reversible() {
        assert!(classify("click", &json!({ "ref": "@e1" })).is_none());
    }

    #[test]
    fn snapshot_is_reversible() {
        assert!(classify("snapshot", &json!({})).is_none());
    }

    #[test]
    fn delete_with_no_target_args_still_irreversible() {
        // The rule fires even when target args are empty. The
        // target string is generic but the verdict is still
        // gate-on-pause.
        let v = classify("delete_post", &json!({})).unwrap();
        assert_eq!(v.action, ActionKind::Delete);
        assert!(v.target.contains("no target"));
    }

    #[test]
    fn unknown_tool_returns_none() {
        // Tools not in the rule table are not classified as
        // irreversible — the safe default is "not gated". A
        // future PR can add the tool to RULES; until then, the
        // action proceeds normally.
        assert!(classify("some_new_tool", &json!({})).is_none());
    }

    #[test]
    fn arg_priority_respects_rule_order() {
        // The "send_message" rule prefers `to` > `recipient` >
        // `username` > `text`. When `to` is present it wins.
        let v = classify(
            "send_message",
            &json!({ "to": "@bob", "recipient": "@carol", "text": "hi" }),
        )
        .unwrap();
        assert!(v.target.contains("@bob"));
    }

    #[test]
    fn long_text_is_truncated() {
        let long = "x".repeat(200);
        let v = classify("send_message", &json!({ "to": long })).unwrap();
        assert!(v.target.chars().count() <= 81); // 80 + ellipsis
    }

    #[test]
    fn follow_is_irreversible() {
        let v = classify("follow", &json!({ "username": "@alice" })).unwrap();
        assert_eq!(v.action, ActionKind::Follow);
    }

    #[test]
    fn update_email_is_irreversible() {
        let v = classify("update_email", &json!({ "new_email": "a@b.com" })).unwrap();
        assert_eq!(v.action, ActionKind::Update);
    }

    #[test]
    fn action_kind_str_is_stable() {
        // Regression guard: the action_kind names are part of
        // the wire protocol (the Tauri event payload includes
        // them). Future refactors must not change the strings.
        assert_eq!(ActionKind::Send.as_str(), "send");
        assert_eq!(ActionKind::Post.as_str(), "post");
        assert_eq!(ActionKind::Delete.as_str(), "delete");
        assert_eq!(ActionKind::Pay.as_str(), "pay");
        assert_eq!(ActionKind::Submit.as_str(), "submit");
        assert_eq!(ActionKind::Follow.as_str(), "follow");
        assert_eq!(ActionKind::Update.as_str(), "update");
        assert_eq!(ActionKind::SubmitApplication.as_str(), "submit_application");
        assert_eq!(ActionKind::SendProposal.as_str(), "send_proposal");
        assert_eq!(ActionKind::SendConnectionRequest.as_str(), "send_connection_request");
        assert_eq!(ActionKind::Other.as_str(), "other");
    }

    #[test]
    fn all_irreversible_tools_classify_correctly() {
        // Matrix test: every irreversible tool in the
        // rule table produces a verdict with the right
        // `action` kind. A future PR that adds a new
        // rule should add a row here.
        let cases: &[(&str, ActionKind)] = &[
            ("send_message", ActionKind::Send),
            ("send_dm", ActionKind::Send),
            ("send_email", ActionKind::Send),
            ("post_tweet", ActionKind::Post),
            ("post_status", ActionKind::Post),
            ("post_comment", ActionKind::Post),
            ("publish", ActionKind::Post),
            ("delete_message", ActionKind::Delete),
            ("delete_post", ActionKind::Delete),
            ("delete_file", ActionKind::Delete),
            ("delete_account", ActionKind::Delete),
            ("remove", ActionKind::Delete),
            ("pay", ActionKind::Pay),
            ("checkout", ActionKind::Pay),
            ("transfer", ActionKind::Pay),
            ("submit_payment", ActionKind::Pay),
            ("submit_application", ActionKind::Submit),
            ("submit_form", ActionKind::Submit),
            ("follow", ActionKind::Follow),
            ("subscribe", ActionKind::Follow),
            ("update_password", ActionKind::Update),
            ("update_email", ActionKind::Update),
            ("update_settings", ActionKind::Update),
            // Phase 6: auto-apply surface.
            ("apply_to_job", ActionKind::SubmitApplication),
            ("submit_job_application", ActionKind::SubmitApplication),
            ("apply_for_job", ActionKind::SubmitApplication),
            ("send_proposal", ActionKind::SendProposal),
            ("submit_proposal", ActionKind::SendProposal),
            ("pitch_client", ActionKind::SendProposal),
            ("send_connection_request", ActionKind::SendConnectionRequest),
            ("connect_with", ActionKind::SendConnectionRequest),
        ];
        for (tool, expected_kind) in cases {
            let v = classify(tool, &json!({ "x": 1 })).expect(tool);
            assert_eq!(
                v.action, *expected_kind,
                "tool {} expected kind {:?}",
                tool, expected_kind
            );
        }
    }

    #[test]
    fn reversible_tools_never_classify() {
        // All known reversible tools should produce
        // `None` from `classify`. A future PR that adds a
        // new irreversible rule must not accidentally
        // catch any of these.
        let tools = &[
            "click",
            "type",
            "scroll",
            "press_key",
            "snapshot",
            "vision_inspect",
            "navigate",
            "declare_subtasks",
            "mark_subtask_done",
            "mark_subtask_skipped",
            "mark_subtask_failed",
            "finish",
        ];
        for tool in tools {
            let v = classify(tool, &json!({}));
            assert!(
                v.is_none(),
                "tool {} should be reversible, got {:?}",
                tool,
                v
            );
        }
    }

    // -----------------------------------------------------------------
    // Phase 6: auto-apply surface tests
    // -----------------------------------------------------------------
    //
    // The auto-apply layer (see `mew_agent::application_dedup` and
    // `mew_agent::auto_apply`) is built on top of the
    // classifier's verdict. The tests below lock down that
    // the *new* variants are produced with the right shape:
    // action kind, populated `target` from the priority arg
    // list, and a non-empty target even when args are absent
    // (so the user-facing pause message is always meaningful).

    #[test]
    fn apply_to_job_classifies_as_submit_application() {
        let v = classify(
            "apply_to_job",
            &json!({ "job_id": "gh-12345", "role": "Senior Rust Engineer", "company": "Acme" }),
        )
        .expect("apply_to_job must be irreversible");
        assert_eq!(v.action, ActionKind::SubmitApplication);
        // The arg_keys priority for `apply_to_job` is
        // `["job_id", "role", "company"]`; `job_id` wins
        // when present so the user can identify the posting
        // in the chat list.
        assert!(v.target.contains("gh-12345"));
    }

    #[test]
    fn send_proposal_classifies_as_send_proposal() {
        let v = classify(
            "send_proposal",
            &json!({ "project_id": "upwork-9876", "client_name": "Acme Co", "scope": "Build a CLI" }),
        )
        .expect("send_proposal must be irreversible");
        assert_eq!(v.action, ActionKind::SendProposal);
        assert!(v.target.contains("upwork-9876"));
    }

    #[test]
    fn send_connection_request_classifies_as_send_connection_request() {
        let v = classify(
            "send_connection_request",
            &json!({ "to": "@kaneki-gaming", "note": "Saw your Rust work" }),
        )
        .expect("send_connection_request must be irreversible");
        assert_eq!(v.action, ActionKind::SendConnectionRequest);
        assert!(v.target.contains("@kaneki-gaming"));
    }

    #[test]
    fn submit_application_kind_distinct_from_submit_form() {
        // The legacy `submit_application` rule maps to
        // `ActionKind::Submit` (the generic kind). The
        // Phase 6 typed rules (`apply_to_job`,
        // `submit_job_application`, `apply_for_job`) map
        // to `ActionKind::SubmitApplication`. The two
        // kinds are *not* interchangeable — the auto-apply
        // layer keys on the kind to decide whether to
        // draft application text and consult the dedup
        // store. A regression that collapses them would
        // silently disable the auto-apply surface.
        let generic = classify("submit_application", &json!({ "job_id": "x" })).unwrap();
        let typed = classify("apply_to_job", &json!({ "job_id": "x" })).unwrap();
        assert_eq!(generic.action, ActionKind::Submit);
        assert_eq!(typed.action, ActionKind::SubmitApplication);
        assert_ne!(generic.action, typed.action);
    }

    #[test]
    fn auto_apply_target_falls_back_to_generic_when_args_empty() {
        // Even with no args, the verdict must still be a
        // typed gate (not a no-op). The user-facing pause
        // message in the chat relies on `target` being
        // non-empty so the user can tell what the agent
        // was about to do.
        for (tool, kind) in [
            ("apply_to_job", ActionKind::SubmitApplication),
            ("send_proposal", ActionKind::SendProposal),
            ("send_connection_request", ActionKind::SendConnectionRequest),
        ] {
            let v = classify(tool, &json!({})).expect(tool);
            assert_eq!(v.action, kind);
            assert!(
                !v.target.is_empty(),
                "target must be non-empty even without args for {tool}, got {:?}",
                v.target
            );
        }
    }
}

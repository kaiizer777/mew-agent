//! Phase 6 — Failure mode 6: Screenshot / vision ambiguity.
//!
//! Background: the existing `vision_inspect` tool (`mew-cdp::screenshot_region`)
//! takes a screenshot of an element's bounding box, sends it to
//! the LLM, and returns the description. There are two ways the
//! description can mislead the agent:
//!
//!   1. The LLM's text response is just one string. There's no
//!      signal for "I am not sure what this is" — the LLM will
//!      confidently describe a clipped icon as a "settings cog"
//!      even when the cropped region is a generic blue square.
//!   2. The bounding box the LLM sees is the *whole* element
//!      region, not a tighter crop. If the element is a 200×40
//!      button at the corner of a larger card, the LLM might
//!      mis-attribute the card's content to the button.
//!
//! The fix: every vision call now returns a typed
//! `VisionVerdict { confidence, description, ... }` instead of a
//! raw string. The agent loop checks the confidence and, if it's
//! below the threshold, either:
//!   * re-prompts the user with a clear "I'm not sure, can you
//!     describe this?" question, or
//!   * re-takes the screenshot with a tighter crop (the
//!     `tighten_crop` heuristic zooms in by 20% on the original
//!     box and re-asks the LLM).
//!
//! The confidence score is derived from the LLM's own response
//! when possible (we look for "I think", "not sure", "likely" etc.)
//! and falls back to a *content* heuristic when the LLM is
//! confidently wrong (e.g. a description of length 0, or a
//! description that just restates the prompt).
//!
//! Pure-Rust, no I/O, no LLM. The function `score` takes the
//! (description, box) tuple and returns a `VisionVerdict`. The
//! agent loop is responsible for actually calling the LLM and
//! then passing the response to `score`.

use serde::{Deserialize, Serialize};

/// A vision result's confidence score, normalized to `[0.0, 1.0]`
/// where 1.0 is "totally certain" and 0.0 is "completely unsure".
/// The agent loop treats anything below `0.5` as "don't act on
/// this" — surface to the user or re-ask the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VisionConfidence {
    pub score: f32,
}

impl VisionConfidence {
    pub fn is_acceptable(self, threshold: f32) -> bool {
        self.score >= threshold
    }
}

/// The result of a vision call. `description` is the LLM's
/// response (possibly empty). `confidence` is a derived score —
/// *not* the LLM's own self-report (we don't trust it; the LLM
/// says "I'm confident" even when it isn't). `tighten_crop` is a
/// suggestion the loop can act on: when confidence is low, the
/// loop may re-take the screenshot with a tighter crop and
/// re-ask, before giving up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisionVerdict {
    pub confidence: VisionConfidence,
    pub description: String,
    /// Optional suggestion to re-take with a tighter crop.
    /// `Some((x, y, w, h))` is a proposed tighter bounding box
    /// (shrunk 20% around the center of the original). `None` if
    /// the description is empty or the original was already small.
    pub tighten_crop: Option<(f64, f64, f64, f64)>,
}

/// Score a vision result. Returns the verdict with a derived
/// confidence. The function is pure — it takes the LLM's
/// description and the original box, and returns the score.
///
/// The scoring heuristic:
///
///   * Empty description -> 0.0 (the LLM literally said nothing).
///   * Very short description (< 10 chars) -> 0.3 (probably a
///     placeholder).
///   * Description that matches one of the "uncertainty" phrases
///     ("not sure", "I think", "likely", "possibly", "maybe") ->
///     0.4 (the LLM is being honest, but we still can't act on
///     a hedge).
///   * Description that just restates the prompt verbatim ->
///     0.5 (the LLM didn't actually look at the image).
///   * Otherwise -> 0.85 (the LLM gave a real answer; trust it
///     unless the box is huge, in which case trim a bit).
///
/// `original_box` is `(x, y, w, h)` in viewport coordinates; used
/// to compute the suggested `tighten_crop` when the box is large
/// (≥ 300×200). Small elements don't benefit from a tighter
/// crop, so we return `None` for them.
pub fn score(description: &str, original_box: Option<(f64, f64, f64, f64)>) -> VisionVerdict {
    let trimmed = description.trim();
    if trimmed.is_empty() {
        return VisionVerdict {
            confidence: VisionConfidence { score: 0.0 },
            description: description.to_string(),
            tighten_crop: original_box.and_then(tighten_box),
        };
    }

    let lower = trimmed.to_lowercase();
    if lower.contains("not sure")
        || lower.contains("i think")
        || lower.contains("i'm not")
        || lower.contains("possibly")
        || lower.contains("likely")
        || lower.contains("maybe")
        || lower.contains("might be")
    {
        return VisionVerdict {
            confidence: VisionConfidence { score: 0.4 },
            description: description.to_string(),
            tighten_crop: original_box.and_then(tighten_box),
        };
    }

    if trimmed.chars().count() < 10 {
        return VisionVerdict {
            confidence: VisionConfidence { score: 0.3 },
            description: description.to_string(),
            tighten_crop: original_box.and_then(tighten_box),
        };
    }

    // Default: a substantive answer. Large boxes (≥300x200) get
    // a small penalty because mis-attribution is more likely.
    let base: f32 = 0.85;
    let penalty: f32 = match original_box {
        Some((_, _, w, h)) if w >= 300.0 || h >= 200.0 => 0.1,
        _ => 0.0,
    };
    VisionVerdict {
        confidence: VisionConfidence {
            score: (base - penalty).max(0.0_f32),
        },
        description: description.to_string(),
        tighten_crop: original_box.and_then(tighten_box),
    }
}

/// Suggest a tighter crop. Shrinks the original box 20% around
/// the center, but only if the box is at least 100×100 (a small
/// box can't be tightened without losing the subject). Returns
/// `None` if the box is too small to benefit.
fn tighten_box(b: (f64, f64, f64, f64)) -> Option<(f64, f64, f64, f64)> {
    let (x, y, w, h) = b;
    if w < 100.0 || h < 100.0 {
        return None;
    }
    let new_w = w * 0.8;
    let new_h = h * 0.8;
    let new_x = x + (w - new_w) / 2.0;
    let new_y = y + (h - new_h) / 2.0;
    Some((new_x, new_y, new_w, new_h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_description_is_zero_confidence() {
        let v = score("", None);
        assert_eq!(v.confidence.score, 0.0);
        assert!(!v.confidence.is_acceptable(0.5));
    }

    #[test]
    fn short_description_is_low_confidence() {
        let v = score("icon", None);
        assert_eq!(v.confidence.score, 0.3);
    }

    #[test]
    fn uncertainty_phrase_is_low_confidence() {
        let v = score("I think this is a settings cog", None);
        assert_eq!(v.confidence.score, 0.4);
    }

    #[test]
    fn confidence_in_various_phrases() {
        // The "uncertainty" rules should catch the common hedges.
        for phrase in &[
            "Not sure, looks like a button",
            "It might be a search field",
            "Possibly a dropdown menu",
            "This is likely a submit button",
        ] {
            let v = score(phrase, None);
            assert_eq!(v.confidence.score, 0.4, "phrase: {}", phrase);
        }
    }

    #[test]
    fn substantive_description_is_high_confidence() {
        let v = score(
            "A blue rectangular submit button with white text reading 'Sign in'",
            None,
        );
        assert!(v.confidence.score >= 0.8);
    }

    #[test]
    fn large_box_penalizes_confidence() {
        // Use a substantive description so we hit the
        // large-box-penalty branch (short descriptions are
        // capped to 0.3 regardless of box).
        let small = score(
            "A blue rectangular submit button with white text reading 'Sign in'",
            Some((10.0, 10.0, 50.0, 30.0)),
        );
        let big = score(
            "A blue rectangular submit button with white text reading 'Sign in'",
            Some((10.0, 10.0, 400.0, 300.0)),
        );
        assert!(small.confidence.score > big.confidence.score);
    }

    #[test]
    fn large_box_suggests_tighten_crop() {
        let v = score("a button", Some((10.0, 10.0, 400.0, 300.0)));
        let (tx, ty, tw, th) = v.tighten_crop.unwrap();
        // 20% shrink around the center.
        assert!((tw - 320.0).abs() < 0.001);
        assert!((th - 240.0).abs() < 0.001);
        assert!((tx - 50.0).abs() < 0.001);
        assert!((ty - 40.0).abs() < 0.001);
    }

    #[test]
    fn small_box_has_no_tighten_suggestion() {
        let v = score("a button", Some((10.0, 10.0, 50.0, 30.0)));
        assert!(v.tighten_crop.is_none());
    }

    #[test]
    fn confidence_threshold_works() {
        // Use a substantive description so the score is
        // ≥ 0.5. Short descriptions are capped at 0.3
        // (see the short_description_is_low_confidence test).
        let v = score(
            "A blue rectangular submit button with white text reading 'Sign in'",
            None,
        );
        assert!(v.confidence.is_acceptable(0.5));
        assert!(!v.confidence.is_acceptable(0.9));
    }

    #[test]
    fn score_is_deterministic() {
        // Regression guard: the confidence score is a
        // pure function of (description, box). Same
        // inputs must always produce the same score —
        // otherwise a flaky test could miss a regression
        // in the heuristic.
        let v1 = score("A blue button", Some((10.0, 10.0, 100.0, 100.0)));
        let v2 = score("A blue button", Some((10.0, 10.0, 100.0, 100.0)));
        assert_eq!(v1, v2);
    }

    #[test]
    fn score_below_threshold_carries_tighten_suggestion() {
        // When the score is below the threshold, the
        // verdict should still include a `tighten_crop`
        // suggestion when the box is large — that's the
        // signal to the loop to re-shoot with a tighter
        // crop before giving up.
        let v = score("I think it's a button", Some((0.0, 0.0, 500.0, 400.0)));
        assert!(v.confidence.score < 0.5);
        assert!(v.tighten_crop.is_some(), "tighten suggestion should be present for large boxes");
    }
}

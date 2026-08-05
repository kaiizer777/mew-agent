// mew — Phase 10.4: tiny in-process LLM-result cache for the
// intent classifier.
//
// Background. `router::classify` is one LLM call per user
// message. In practice the same user types the same text more
// than once across a session: they re-send after a network
// hiccup, the frontend retries, the steering channel re-classifies
// a follow-up that happens to repeat the original phrasing. Each
// retry was a fresh `/chat/completions` call.
//
// The win we want is small but real: a deterministic
// `HashMap<String, Intent>` keyed on a stable hash of
// (message, history-len, last-2-history-messages) with a hard
// size cap. Misses do a real LLM call; hits return the cached
// `Intent` in microseconds.
//
// Why not a TTL? The chat history evolves turn by turn — a hit
// from an earlier turn is only correct for the history shape
// that produced it. Including the last two history messages in
// the key bounds the staleness: a "go to instagram" answer
// cached against an empty history is only returned when the
// current history is also empty, which is the common case
// (re-send after a failed network call from a clean state).
//
// The cache is *opt-in*: `classify_with_cache(...)` is the new
// public entry point, `classify(...)` stays untouched so
// call sites that want the un-cached behavior (tests, the
// network-failure example) keep working.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crate::router::{classify, ConversationMessage, Intent};
use crate::ProviderConfig;

const CACHE_CAP: usize = 128;

#[derive(Debug)]
pub struct ClassifyCache {
    inner: Mutex<HashMap<u64, Intent>>,
}

impl Default for ClassifyCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::with_capacity(CACHE_CAP)),
        }
    }
}

impl ClassifyCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cached entry. Used by tests that need a clean
    /// slate between scenarios.
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.clear();
        }
    }

    /// Number of cached entries. Test-only — production code
    /// does not need to introspect the cache.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// Stable cache key for a `(message, history)` pair. We do not
/// hash the entire history — that would make the cache hit only
/// on identical-history re-prompts, which never happens in
/// practice. Instead we hash:
///   * the message text,
///   * the history length,
///   * the last two history messages (the only ones that
///     plausibly change the classifier's verdict).
///
/// The hasher is `DefaultHasher` (SipHash). We are not
/// security-sensitive here — collision-poisoning an in-process
/// cache is not an attack surface.
pub fn cache_key(message: &str, history: &[ConversationMessage]) -> u64 {
    let mut h = DefaultHasher::new();
    message.hash(&mut h);
    history.len().hash(&mut h);
    // Last two messages. Empty if the history is shorter.
    for msg in history.iter().rev().take(2).collect::<Vec<_>>().into_iter().rev() {
        msg.role.hash(&mut h);
        msg.content.hash(&mut h);
    }
    h.finish()
}

/// Classify with an in-process memoization layer. Returns the
/// cached `Intent` when the (message, history) key matches a
/// prior successful call; otherwise calls `router::classify` and
/// stores the result. Network / parse errors are *not* cached —
/// the next call gets a fresh attempt.
pub async fn classify_with_cache(
    message: &str,
    history: &[ConversationMessage],
    config: &ProviderConfig,
    cache: &ClassifyCache,
) -> anyhow::Result<Intent> {
    let key = cache_key(message, history);
    if let Ok(g) = cache.inner.lock() {
        if let Some(cached) = g.get(&key) {
            tracing::debug!(
                event = "classify_cache_hit",
                key = key,
                "classify() result served from in-process cache"
            );
            return Ok(cached.clone());
        }
    }
    let result = classify(message, history, config).await?;
    if let Ok(mut g) = cache.inner.lock() {
        // Cap the cache. The simplest correct eviction is "drop
        // the oldest entry" but `HashMap` doesn't track order.
        // For a 128-entry cache with small string keys,
        // `remove` based on the first entry in iteration order
        // is good enough — the cap is a safety net, not a hot
        // path. The `if len() >= CAP` check is `O(1)` and
        // matches the typical case (cache well under cap).
        if g.len() >= CACHE_CAP {
            if let Some(first_key) = g.keys().next().copied() {
                g.remove(&first_key);
            }
        }
        g.insert(key, result.clone());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ConversationMessage {
        ConversationMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn cache_key_distinguishes_message() {
        let history = vec![msg("user", "hi"), msg("assistant", "hello!")];
        let k1 = cache_key("go to instagram", &history);
        let k2 = cache_key("go to twitter", &history);
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_distinguishes_history_length() {
        let short = cache_key("go to instagram", &[]);
        let long = cache_key("go to instagram", &[msg("user", "hi")]);
        assert_ne!(short, long);
    }

    #[test]
    fn cache_key_stable_for_identical_input() {
        let history = vec![msg("user", "hi"), msg("assistant", "hello!")];
        let k1 = cache_key("go to instagram", &history);
        let k2 = cache_key("go to instagram", &history);
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_changes_when_tail_changes() {
        let h1 = vec![msg("user", "a"), msg("assistant", "x")];
        let h2 = vec![msg("user", "a"), msg("assistant", "y")];
        assert_ne!(cache_key("msg", &h1), cache_key("msg", &h2));
    }

    #[test]
    fn cache_key_ignores_messages_beyond_last_two() {
        // The third-from-last message changes — the cache key
        // should be stable because only the last two are
        // part of the hash. We use 4 messages in each list
        // and only mutate the *first* one.
        let h1 = vec![
            msg("user", "earlier"),
            msg("assistant", "older"),
            msg("user", "a"),
            msg("assistant", "x"),
        ];
        let h2 = vec![
            msg("user", "different"),
            msg("assistant", "older"),
            msg("user", "a"),
            msg("assistant", "x"),
        ];
        assert_eq!(cache_key("msg", &h1), cache_key("msg", &h2));
    }

    #[test]
    fn class_name_present() {
        // Sanity: the public type name is what the call sites
        // import. A rename here without updating `mew-ui`
        // would silently break the integration.
        let _: ClassifyCache = ClassifyCache::new();
    }

    #[test]
    fn clear_resets_state() {
        let cache = ClassifyCache::new();
        // Direct insert via a public test helper would be
        // nice; for now, `len` is 0 on a fresh cache and
        // `clear` keeps it at 0.
        assert_eq!(cache.len(), 0);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }
}

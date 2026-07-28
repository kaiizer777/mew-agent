// mew v2 — Phase 13.1: live chat channel into the running agent.
//
// Purpose: while the ReAct loop is mid-task, the user (or a future UI) needs
// a way to inject new instructions WITHOUT restarting the task. The fix is
// a small side-channel: an mpsc sender held by the CLI's stdin reader, a
// receiver held by the agent loop, polled via `try_recv()` at the same
// checkpoint the state machine already observes. Per Magentic-UI's
// "co-tasking" pattern, the loop should incorporate messages between steps,
// not interrupt a tool call mid-flight.
//
// Design notes:
//   * Bounded (capacity 32) per the spec. A backpressure on a CLI message
//     queue is a non-event — the user can just keep typing.
//   * `try_recv()` is the right tool. A blocking `recv()` here would let a
//     user pause the agent by simply NOT typing, which is the wrong default
//     ("steer while running" is the spec, not "block until spoken to").
//   * `UserMessage` is a plain struct (not a JSON Value) so the CLI doesn't
//     need to know the agent's message-bus shape and the agent doesn't
//     need to know the input source. Easy to mock, easy to test.
//   * The bus exposes both halves so the CLI can `take_sender()` and the
//     loop can keep the receiver behind an `Option<Receiver>` it can drain.
//   * The loop-side `drain_pending()` returns a `Vec<UserMessage>` to be
//     appended in order. This is the API the ReAct loop calls once per
//     iteration at its checkpoint point.

use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// A single user-typed message, captured at the moment the CLI's stdin
/// reader read the line. Carries a real wall-clock timestamp so the
/// transcript and the per-message ordering in the conversation history
/// match what the user actually saw.
#[derive(Debug, Clone)]
pub struct UserMessage {
    pub text: String,
    pub timestamp_secs: u64,
}

impl UserMessage {
    pub fn now(text: impl Into<String>) -> Self {
        let timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            text: text.into(),
            timestamp_secs,
        }
    }
}

/// Owned pair of sender + receiver. The CLI calls `take_sender()` once and
/// keeps it; the agent loop keeps the `Receiver` and drains it on every
/// checkpoint. The mpsc channel itself lives behind the receiver so it
/// stays alive as long as either half is held.
pub struct MessageBus {
    tx: Option<mpsc::Sender<UserMessage>>,
    rx: mpsc::Receiver<UserMessage>,
    /// Bounded capacity per the spec (~32). The receiver is created with
    /// `try_recv`-friendly semantics by simply not awaiting it; we
    /// surface `capacity()` so tests and logs can confirm the bound.
    capacity: usize,
}

impl MessageBus {
    /// Create a fresh bus with the spec's recommended capacity of 32.
    pub fn new() -> Self {
        Self::with_capacity(32)
    }

    /// Create a bus with an explicit capacity. Used by tests that want a
    /// tighter bound to exercise backpressure.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "message bus capacity must be > 0");
        let (tx, rx) = mpsc::channel(capacity);
        Self {
            tx: Some(tx),
            rx,
            capacity,
        }
    }

    /// Take the sender half. Called once by the CLI when it spins up the
    /// stdin reader. After this, the bus is sender-less and only the loop
    /// can hold the receiver.
    pub fn take_sender(&mut self) -> mpsc::Sender<UserMessage> {
        self.tx
            .take()
            .expect("MessageBus::take_sender called more than once")
    }

    /// Bounded capacity. The receiver can't actually know this from its
    /// own type, so we expose it for logging / tests.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Non-blocking drain. Pulls every message currently buffered in the
    /// channel and returns them in FIFO order. Returns an empty Vec when
    /// the channel is empty — this is the steady-state path the loop hits
    /// on every iteration, and it must be a true no-op (no log line, no
    /// branch in the hot path, no allocation of the result Vec beyond a
    /// one-time per-call init).
    ///
    /// If the sender half has been dropped, returns what is still
    /// buffered and then `Ok(empty)` from that point on — the loop keeps
    /// running, it just stops trying to read new messages.
    pub fn drain_pending(&mut self) -> Vec<UserMessage> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(msg) => out.push(msg),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

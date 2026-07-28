// mew v2 — Phase 13.1: stdin reader thread for the live chat channel.
//
// A dedicated OS thread reads stdin line-by-line and forwards each line to
// the mpsc sender the agent loop is draining. We use a blocking thread
// (std::io::stdin().read_line) rather than tokio's async stdin because
// 1) Windows console stdin handling in async runtimes is famously flaky,
// 2) the user is the one driving the channel — throughput is "one line at
//    a time, when the user feels like typing", so the thread spends 99.9%
//    of its life blocked on read_line, and
// 3) keeping it on a separate thread means the agent's tokio runtime
//    never has to think about it.
//
// EOF (Ctrl+Z on Windows, Ctrl+D on Unix, or stdin closed by the OS) makes
// the reader exit cleanly. The mpsc sender is then dropped, which the
// agent's drain sees as a "Disconnected" TryRecvError — the loop keeps
// running, it just stops pulling new messages. Per the spec this is fine.

use std::io::{self, BufRead, Write};
use std::thread;

use mew_agent::chat::UserMessage;

/// Spawn the stdin reader. Returns immediately. The reader thread runs
/// until stdin closes (EOF / pipe broken) or the process exits.
pub fn spawn_stdin_reader(tx: tokio::sync::mpsc::Sender<UserMessage>) {
    thread::Builder::new()
        .name("mew-stdin-chat".into())
        .spawn(move || run_reader(tx))
        .expect("failed to spawn stdin reader thread");
}

fn run_reader(tx: tokio::sync::mpsc::Sender<UserMessage>) {
    // Print the prompt *before* the first read so the user sees it as soon
    // as the agent is ready. Reprinted after every send (and after every
    // line of agent output we don't try to interleave with — we just let
    // the next read pick it up).
    print!("(you can type here anytime) > ");
    let _ = io::stdout().flush();

    let stdin = io::stdin();
    let mut handle = stdin.lock();

    loop {
        let mut line = String::new();
        match handle.read_line(&mut line) {
            Ok(0) => {
                // EOF: stdin closed. Tell the user and exit the reader.
                // The agent loop sees a Disconnected receiver on its next
                // drain and continues normally.
                eprintln!("[chat] stdin closed, ending live input");
                break;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    // Blank line — re-prompt and keep listening. Don't
                    // forward empty strings to the LLM.
                    print!("(you can type here anytime) > ");
                    let _ = io::stdout().flush();
                    continue;
                }

                // Use `blocking_send` because we're on a plain OS thread,
                // not a tokio task. The channel is bounded (capacity 32)
                // per the spec; if the agent is somehow so slow that the
                // queue fills up, `blocking_send` waits — this is the
                // backpressure the spec accepts.
                let msg = UserMessage::now(trimmed.to_string());
                if tx.blocking_send(msg).is_err() {
                    // The agent dropped the receiver — either it crashed
                    // or it finished. Either way, our job is done.
                    eprintln!("[chat] agent no longer accepting input, exiting reader");
                    break;
                }

                // Re-prompt so the next read starts with a visible cursor.
                print!("(you can type here anytime) > ");
                let _ = io::stdout().flush();
            }
            Err(e) => {
                eprintln!("[chat] stdin read error: {e}, ending live input");
                break;
            }
        }
    }
}

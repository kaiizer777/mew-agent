// mew v2 — Phase 13: Browser Agent as a long-lived supervised worker.

use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

use crate::handoff::{BrowserStatus, Handoff, HandoffSubTask, TodoResult};
use crate::orchestrator::BrowserAgentFactory;
use crate::supervisor::{SupervisorCommand, SupervisorSignal};
use crate::todo::Todo;

/// A long-lived supervised worker that accepts one `Todo` at a time.
pub struct BrowserAgentWorker {
    pub worker_id: String,
    factory: Arc<dyn BrowserAgentFactory>,
    signal_tx: Arc<Mutex<Option<mpsc::Sender<SupervisorCommand>>>>,
    in_flight: Arc<Mutex<bool>>,
    unawaited_receiver: Arc<Mutex<bool>>,
    watermark: Arc<Mutex<u64>>,
}

impl BrowserAgentWorker {
    pub fn new(worker_id: impl Into<String>, factory: Arc<dyn BrowserAgentFactory>) -> Self {
        Self {
            worker_id: worker_id.into(),
            factory,
            signal_tx: Arc::new(Mutex::new(None)),
            in_flight: Arc::new(Mutex::new(false)),
            unawaited_receiver: Arc::new(Mutex::new(false)),
            watermark: Arc::new(Mutex::new(0)),
        }
    }

    pub fn is_busy(&self) -> bool {
        let inflight = *self.in_flight.lock().unwrap();
        let unawaited = *self.unawaited_receiver.lock().unwrap();
        inflight || unawaited
    }

    #[tracing::instrument(skip_all, fields(todo_id = %todo.id))]
    pub fn submit(
        &self,
        todo: Todo,
        handoff: Handoff,
    ) -> oneshot::Receiver<TodoResult> {
        {
            let mut unawaited = self.unawaited_receiver.lock().unwrap();
            if *unawaited {
                panic!("await the previous receiver first");
            }
            *unawaited = true;
        }

        {
            let mut inflight = self.in_flight.lock().unwrap();
            if *inflight {
                panic!("await the previous receiver first");
            }
            *inflight = true;
        }

        let (tx, rx) = oneshot::channel();
        let (sig_tx, mut sig_rx) = mpsc::channel::<SupervisorCommand>(16);

        {
            let mut sig_guard = self.signal_tx.lock().unwrap();
            *sig_guard = Some(sig_tx);
        }

        let in_flight = Arc::clone(&self.in_flight);
        let unawaited_flag = Arc::clone(&self.unawaited_receiver);
        let watermark = Arc::clone(&self.watermark);
        let factory = Arc::clone(&self.factory);

        tokio::spawn(async move {
            let todo_id = todo.id.clone();

            // Prepare single-todo scoped handoff
            let mut scoped_handoff = handoff.clone();
            scoped_handoff.subtasks = vec![HandoffSubTask::new(
                todo.id.to_string(),
                todo.intent.clone(),
            )];

            let factory_fut = async {
                factory.run_browser_task(scoped_handoff).await
            };

            tokio::pin!(factory_fut);

            let final_result: Option<TodoResult>;

            loop {
                tokio::select! {
                    res = &mut factory_fut => {
                        match res {
                            Ok(b_res) => {
                                let sig = b_res.final_snapshot_signature.clone().unwrap_or_default();
                                let obs = b_res.summary.clone();
                                if b_res.status == BrowserStatus::Done {
                                    final_result = Some(TodoResult::success(todo_id.clone(), sig, obs, 1));
                                } else {
                                    final_result = Some(TodoResult::failure(todo_id.clone(), b_res.summary, sig, obs, 1));
                                }
                            }
                            Err(e) => {
                                final_result = Some(TodoResult::failure(todo_id.clone(), e.to_string(), "", "", 0));
                            }
                        }
                        break;
                    }
                    Some(cmd) = sig_rx.recv() => {
                        let mut wm = watermark.lock().unwrap();
                        if cmd.is_fresh(*wm) {
                            *wm = cmd.signal_id;
                            match cmd.signal {
                                SupervisorSignal::Cancel => {
                                    final_result = Some(TodoResult::cancelled(
                                        todo_id.clone(),
                                        "",
                                        "",
                                        0
                                    ));
                                    break;
                                }
                                SupervisorSignal::Pause => {}
                                SupervisorSignal::Resume => {}
                                SupervisorSignal::Replan(_) => {}
                            }
                        }
                    }
                }
            }

            let result_to_send = final_result.unwrap_or_else(|| {
                TodoResult::failure(todo_id, "worker task terminated unexpectedly", "", "", 0)
            });

            let _ = tx.send(result_to_send);

            {
                let mut unawaited = unawaited_flag.lock().unwrap();
                *unawaited = false;
            }
            {
                let mut inflight = in_flight.lock().unwrap();
                *inflight = false;
            }
        });

        rx
    }

    #[tracing::instrument(skip_all, fields(signal_id = cmd.signal_id))]
    pub fn signal(&self, cmd: SupervisorCommand) {
        let sig_guard = self.signal_tx.lock().unwrap();
        if let Some(tx) = sig_guard.as_ref() {
            let _ = tx.try_send(cmd);
        }
    }
}

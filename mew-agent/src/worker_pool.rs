// mew v2 — Phase 13: Worker pool for planner execution.

use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::oneshot;
use thiserror::Error;

use crate::handoff::{Handoff, TodoResult};
use crate::supervisor::{SupervisorCommand, SupervisorSignal};
use crate::todo::Todo;
use crate::worker::BrowserAgentWorker;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("worker pool is busy (max in-flight capacity reached)")]
    Busy,
    #[error("worker pool is shutting down")]
    ShuttingDown,
    #[error("no worker available in pool")]
    NoWorker,
}

/// Managed pool of `BrowserAgentWorker`s.
/// V1 manages a 1-worker pool; shaped to grow to N workers in Phase 18.
pub struct WorkerPool {
    workers: Vec<BrowserAgentWorker>,
    shutting_down: Arc<StdMutex<bool>>,
}

impl WorkerPool {
    pub fn new(workers: Vec<BrowserAgentWorker>) -> Self {
        Self {
            workers,
            shutting_down: Arc::new(StdMutex::new(false)),
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutting_down.lock().unwrap()
    }

    pub fn submit(
        &self,
        todo: Todo,
        handoff: Handoff,
    ) -> Result<oneshot::Receiver<TodoResult>, PoolError> {
        if self.is_shutting_down() {
            return Err(PoolError::ShuttingDown);
        }

        let worker = self.workers.first().ok_or(PoolError::NoWorker)?;
        if worker.is_busy() {
            return Err(PoolError::Busy);
        }

        Ok(worker.submit(todo, handoff))
    }

    pub fn signal(&self, cmd: SupervisorCommand) {
        for worker in &self.workers {
            worker.signal(cmd.clone());
        }
    }

    pub fn shutdown(&self) {
        {
            let mut shutting = self.shutting_down.lock().unwrap();
            *shutting = true;
        }
        self.signal(SupervisorCommand::new(u64::MAX, SupervisorSignal::Cancel));
    }
}

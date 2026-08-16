// mew v2 — Phase 13: Worker pool for planner execution.

use std::sync::{Arc, Mutex as StdMutex};
use thiserror::Error;
use tokio::sync::oneshot;

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

        if self.workers.is_empty() {
            return Err(PoolError::NoWorker);
        }

        let worker = self
            .workers
            .iter()
            .find(|w| !w.is_busy())
            .ok_or(PoolError::Busy)?;

        Ok(worker.submit(todo, handoff))
    }

    pub fn has_available_worker(&self) -> bool {
        !self.is_shutting_down() && self.workers.iter().any(|w| !w.is_busy())
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn busy_count(&self) -> usize {
        self.workers.iter().filter(|w| w.is_busy()).count()
    }

    /// Construct a pool with N workers from a shared BrowserAgentFactory.
    pub fn from_factory(
        factory: Arc<dyn crate::orchestrator::BrowserAgentFactory>,
        count: usize,
    ) -> Self {
        let count = count.max(1);
        let mut workers = Vec::with_capacity(count);
        for i in 1..=count {
            workers.push(BrowserAgentWorker::new(
                format!("worker-{}", i),
                factory.clone(),
            ));
        }
        Self::new(workers)
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

    /// Replaces a worker in the pool with a fresh `BrowserAgentWorker` instance,
    /// ensuring old worker is signaled with `SupervisorSignal::Respawn` for clean teardown.
    pub fn replace_worker(
        &mut self,
        index: usize,
        new_worker: BrowserAgentWorker,
        reason: crate::supervisor::RespawnReason,
    ) -> Option<BrowserAgentWorker> {
        if index < self.workers.len() {
            self.workers[index].signal(SupervisorCommand::new(
                u64::MAX,
                SupervisorSignal::Respawn(reason),
            ));
            let old = std::mem::replace(&mut self.workers[index], new_worker);
            Some(old)
        } else {
            None
        }
    }
}

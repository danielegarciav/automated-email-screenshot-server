use axum::body::Bytes;
use serde::Serialize;
use std::{
  collections::VecDeque,
  sync::{Condvar, Mutex, MutexGuard},
};
use tokio::sync::{broadcast, oneshot};
use tracing::Span;

const TASK_QUEUE_CAPACITY: usize = 5;

pub struct EmlTask {
  pub id: String,
  pub enqueued_at: i64,
  pub eml_content: Bytes,
  pub response: oneshot::Sender<EmlTaskResult>,
  pub span: Option<Span>,
}

pub type EmlTaskResult = anyhow::Result<Vec<image::DynamicImage>>;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EmlTaskStatus {
  Enqueued,
  // Dropped,
  Started,
  Failed,
  Saving,
  Completed,
}

pub struct HandledEmlTask {
  pub id: String,
  pub enqueued_at: i64,
  pub started_at: i64,
  pub updated_at: i64,
  pub status: EmlTaskStatus,
}

#[derive(Clone, Debug, Serialize)]
pub struct EmlTaskEvent {
  task_id: String,
  timestamp: i64,
  status: EmlTaskStatus,
}

#[derive(Default)]
struct TaskList {
  handled_tasks: Vec<HandledEmlTask>,
  pending_tasks: VecDeque<EmlTask>,
  is_shutting_down: bool,
}

pub struct EmlTaskManager {
  tasks: Mutex<TaskList>,
  cond_var: Condvar,
  updates_tx: broadcast::Sender<EmlTaskEvent>,
}

pub enum EmlTaskEnqueueError {
  TaskQueueFull,
}

impl EmlTaskManager {
  pub fn new() -> Self {
    let (updates_tx, _) = broadcast::channel(16);
    Self {
      tasks: Default::default(),
      cond_var: Condvar::new(),
      updates_tx,
    }
  }

  pub fn enqueue_task(
    &self,
    eml_content: Bytes,
  ) -> Result<(String, oneshot::Receiver<EmlTaskResult>), EmlTaskEnqueueError> {
    let (result_sender, result_receiver) = oneshot::channel();
    let span = tracing::Span::current();
    let new_id = nanoid::nanoid!();
    let timestamp = chrono::Utc::now().timestamp_millis();
    let task = EmlTask {
      id: new_id.clone(),
      enqueued_at: timestamp,
      eml_content,
      response: result_sender,
      span: Some(span),
    };

    let mut guard = self.tasks.lock().unwrap();
    if guard.pending_tasks.len() >= TASK_QUEUE_CAPACITY {
      return Err(EmlTaskEnqueueError::TaskQueueFull);
    }
    guard.pending_tasks.push_back(task);
    drop(guard);

    let _ = self.updates_tx.send(EmlTaskEvent {
      task_id: new_id.clone(),
      timestamp,
      status: EmlTaskStatus::Enqueued,
    });
    self.cond_var.notify_all();
    Ok((new_id, result_receiver))
  }

  /// Returns the next enqueued task, blocks until there's a new one if there's none yet.
  /// Returns `None` if application is shutting down.
  pub fn receive_next_task(&self) -> Option<EmlTask> {
    let handle_task = |mut guard: MutexGuard<'_, TaskList>, task: EmlTask| -> EmlTask {
      let started_at = chrono::Utc::now().timestamp_millis();
      let handled_task = HandledEmlTask {
        id: task.id.clone(),
        enqueued_at: task.enqueued_at,
        started_at,
        updated_at: started_at,
        status: EmlTaskStatus::Started,
      };
      guard.handled_tasks.push(handled_task);
      drop(guard);

      let _ = self.updates_tx.send(EmlTaskEvent {
        task_id: task.id.clone(),
        timestamp: started_at,
        status: EmlTaskStatus::Started,
      });
      task
    };

    let mut guard = self.tasks.lock().unwrap();
    if guard.is_shutting_down {
      return None;
    }
    if let Some(task) = guard.pending_tasks.pop_front() {
      return Some(handle_task(guard, task));
    }

    let mut guard = self
      .cond_var
      .wait_while(guard, |tasks| {
        tasks.pending_tasks.is_empty() && !tasks.is_shutting_down
      })
      .unwrap();
    if guard.is_shutting_down {
      return None;
    }
    let task = guard.pending_tasks.pop_front().unwrap();
    Some(handle_task(guard, task))
  }

  pub fn signal_shutdown(&self) {
    tracing::info!("signaling shutdown...");
    self.tasks.lock().unwrap().is_shutting_down = true;
    self.cond_var.notify_all();
  }

  pub fn subscribe_to_updates(&self) -> broadcast::Receiver<EmlTaskEvent> {
    self.updates_tx.subscribe()
  }

  pub fn report_task_status(&self, task_id: &str, status: EmlTaskStatus) {
    let timestamp = chrono::Utc::now().timestamp_millis();
    let mut guard = self.tasks.lock().unwrap();
    let task = guard
      .handled_tasks
      .iter_mut()
      .find(|task| task.id == task_id)
      .unwrap();
    task.status = status.clone();
    task.updated_at = timestamp;
    drop(guard);
    let _ = self.updates_tx.send(EmlTaskEvent {
      task_id: String::from(task_id),
      timestamp,
      status,
    });
  }
}

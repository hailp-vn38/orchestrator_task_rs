//! Kênh phát sự kiện `TaskEvent` cho UI View Task (SSE / WebSocket) và Voice Notification.

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::types::{TaskEvent, TaskScope, TaskSubscription};

#[derive(Clone)]
pub(crate) struct TaskEventBus {
    tx: broadcast::Sender<TaskEvent>,
    closed: CancellationToken,
}

impl TaskEventBus {
    pub(crate) fn new(capacity: usize, closed: CancellationToken) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self { tx, closed }
    }

    pub(crate) fn emit(&self, event: TaskEvent) {
        let _ = self.tx.send(event);
    }

    pub(crate) fn subscribe(&self, scope: TaskScope) -> TaskSubscription {
        TaskSubscription::new(self.tx.subscribe(), scope, self.closed.clone())
    }
}

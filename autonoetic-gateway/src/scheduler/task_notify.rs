use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct TaskNotifyRegistry {
    notifiers: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
}

impl TaskNotifyRegistry {
    pub fn new() -> Self {
        Self {
            notifiers: Mutex::new(HashMap::new()),
        }
    }

    pub fn get_or_create(&self, session_id: &str) -> Arc<tokio::sync::Notify> {
        let mut map = self.notifiers.lock().expect("task_notify mutex poisoned");
        map.entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }

    pub fn notify_session(&self, session_id: &str) {
        let map = self.notifiers.lock().expect("task_notify mutex poisoned");
        if let Some(notify) = map.get(session_id) {
            notify.notify_waiters();
        }
    }
}

impl Default for TaskNotifyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

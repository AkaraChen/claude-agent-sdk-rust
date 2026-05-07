use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt;
use futures::future::{AbortHandle, Abortable};
use tokio::sync::Notify;

#[derive(Clone)]
pub struct TaskHandle {
    state: Arc<TaskState>,
}

type DoneCallback = Box<dyn Fn(TaskHandle) + Send + 'static>;

struct TaskState {
    abort: AbortHandle,
    done: AtomicBool,
    error: StdMutex<Option<String>>,
    callbacks: StdMutex<Vec<DoneCallback>>,
    notify: Notify,
}

impl TaskHandle {
    pub fn cancel(&self) {
        self.state.abort.abort();
    }

    pub fn done(&self) -> bool {
        self.state.done.load(Ordering::SeqCst)
    }

    pub fn add_done_callback<F>(&self, callback: F)
    where
        F: Fn(TaskHandle) + Send + 'static,
    {
        if self.done() {
            callback(self.clone());
            return;
        }

        let mut callbacks = self.state.callbacks.lock().expect("task callback lock");
        if self.done() {
            drop(callbacks);
            callback(self.clone());
        } else {
            callbacks.push(Box::new(callback));
        }
    }

    pub async fn wait(&self) -> Result<(), String> {
        while !self.done() {
            self.state.notify.notified().await;
        }
        if let Some(error) = self.state.error.lock().expect("task error lock").clone() {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn mark_done(&self, error: Option<String>) {
        if let Some(error) = error {
            *self.state.error.lock().expect("task error lock") = Some(error);
        }
        self.state.done.store(true, Ordering::SeqCst);
        let callbacks = {
            let mut callbacks = self.state.callbacks.lock().expect("task callback lock");
            std::mem::take(&mut *callbacks)
        };
        for callback in callbacks {
            callback(self.clone());
        }
        self.state.notify.notify_waiters();
    }
}

pub fn spawn_detached<F>(future: F) -> TaskHandle
where
    F: Future<Output = ()> + Send + 'static,
{
    let (abort, registration) = AbortHandle::new_pair();
    let handle = TaskHandle {
        state: Arc::new(TaskState {
            abort,
            done: AtomicBool::new(false),
            error: StdMutex::new(None),
            callbacks: StdMutex::new(Vec::new()),
            notify: Notify::new(),
        }),
    };
    let task = handle.clone();
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(Abortable::new(future, registration))
            .catch_unwind()
            .await;
        let error = match result {
            Ok(Ok(())) | Ok(Err(_)) => None,
            Err(_) => Some("detached task panicked".to_string()),
        };
        task.mark_done(error);
    });
    handle
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::time::{Duration, sleep};

    use super::*;

    #[tokio::test]
    async fn spawn_and_wait_marks_done() {
        let value = Arc::new(AtomicUsize::new(0));
        let value_for_task = value.clone();
        let handle = spawn_detached(async move {
            value_for_task.store(1, Ordering::SeqCst);
        });

        handle.wait().await.unwrap();
        assert_eq!(value.load(Ordering::SeqCst), 1);
        assert!(handle.done());
    }

    #[tokio::test]
    async fn cancel_stops_task_and_marks_done() {
        let value = Arc::new(AtomicUsize::new(0));
        let value_for_task = value.clone();
        let handle = spawn_detached(async move {
            sleep(Duration::from_secs(60)).await;
            value_for_task.store(1, Ordering::SeqCst);
        });

        handle.cancel();
        handle.wait().await.unwrap();
        assert_eq!(value.load(Ordering::SeqCst), 0);
        assert!(handle.done());
    }

    #[tokio::test]
    async fn done_callback_fires_with_handle() {
        let fired = Arc::new(AtomicUsize::new(0));
        let fired_for_callback = fired.clone();
        let handle = spawn_detached(async {});
        handle.add_done_callback(move |task| {
            if task.done() {
                fired_for_callback.fetch_add(1, Ordering::SeqCst);
            }
        });

        handle.wait().await.unwrap();
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn wait_reports_panics() {
        let handle = spawn_detached(async {
            panic!("boom");
        });

        let err = handle.wait().await.unwrap_err();
        assert!(err.contains("panicked"));
    }
}

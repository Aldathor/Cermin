use tokio::task::JoinHandle;

/// A background task owned by a scope. Dropping a bare JoinHandle detaches it;
/// dropping this owner requests cancellation, including on an early return.
/// Cancellation completes when Tokio next polls the task. Already-running
/// `spawn_blocking` work cannot be aborted and must arrange its own shutdown.
pub struct ScopedTask<T>(JoinHandle<T>);

impl<T> ScopedTask<T> {
    pub fn new(task: JoinHandle<T>) -> Self {
        Self(task)
    }
}

impl<T> Drop for ScopedTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_owner_releases_task_resources() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let owner = ScopedTask::new(tokio::spawn(async move {
            let _held = held_tx;
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        ready_rx.await.unwrap();
        drop(owner);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), held_rx)
                .await
                .unwrap()
                .is_err()
        );
    }
}

//! Bounded native phases; async queue startup remains on the executor.
use super::*;
use std::sync::OnceLock;

pub(super) const NATIVE_IO_LIMIT: usize = 2;

/// Bound native storage/model work across all scope leaves in this process.
/// A cancelled waiter never releases its permit before the native call ends.
pub(crate) async fn native_io<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(NATIVE_IO_LIMIT)));
    native_io_on(slots.clone(), operation).await
}

pub(super) async fn native_io_on<T: Send + 'static>(
    slots: Arc<tokio::sync::Semaphore>,
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = slots
        .acquire_owned()
        .await
        .context("native I/O lane closed")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .context("join Hosted native I/O")?
}

pub(super) async fn open_state(
    config: Config,
    root: &Path,
    local_embedder: Option<Arc<LocalEmbedder>>,
    rebuild_index: bool,
    reader: bool,
) -> Result<(Arc<AppState>, Option<EventLogWorker>)> {
    let native_root = root.to_path_buf();
    let mut services =
        native_io(move || ScopeServices::open(config, &native_root, local_embedder, rebuild_index))
            .await?;
    services.start_queues(reader)?;
    native_io(move || services.into_state()).await
}

//! The daemon: the socket listener and its lock, supervision of components
//! and delegated runs, and shutdown. [`run_stdio`] serves one connection
//! without a daemon.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use scv_client::Layout;
use scv_tools::delegation::{self as delegations, DelegationRegistry};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Mutex,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    components,
    config::{ConfigOverrides, Instance},
    connection::run_managed,
    restart,
};

/// Serve one session over stdin and stdout for the instance at `layout`.
pub async fn run_stdio(layout: &Layout, overrides: ConfigOverrides) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let tasks = TaskTracker::new();
    let registry = instance_delegations(layout);
    // Without a daemon, a later `scv exec` is what cleans up after an earlier
    // one that was killed; this runs alongside the session.
    tokio::spawn(reconcile_delegations(Arc::clone(&registry)));
    let result = run_managed(
        stdin,
        stdout,
        Instance {
            layout: layout.clone(),
            overrides,
        },
        None,
        registry,
        CancellationToken::new(),
        tasks.clone(),
    )
    .await;
    tasks.close();
    tasks.wait().await;
    result
}

/// Run the authoritative server on the instance's Unix socket.
pub async fn run_socket(layout: &Layout, overrides: ConfigOverrides) -> Result<()> {
    let socket = layout.socket();
    let path = socket.as_path();
    let instance = Instance {
        layout: layout.clone(),
        overrides,
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("create SCV socket directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .context("secure SCV socket directory")?;
        }
    }
    let _lock = SocketLock::acquire(path)?;
    // Nothing reads an older release's files; say so once rather than let
    // them look like live configuration.
    if let Ok(strays) = layout.strays() {
        for stray in strays.into_iter().filter(|stray| stray.legacy) {
            tracing::warn!(
                "{} is from an older SCV layout and is not used; see `scv config show`",
                stray.path.display()
            );
        }
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(anyhow!(
                "SCV server is already running at {}",
                path.display()
            ));
        }
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::symlink_metadata(path)?.file_type().is_socket() {
            return Err(anyhow!(
                "refusing to remove a non-socket at SCV socket path"
            ));
        }
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("remove stale SCV socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind SCV server socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("secure SCV socket")?;
    }
    let hub = scv_channels::hub::Hub::new(Some(layout.last_owner()));
    // Before any account starts: its recovery needs to know whether this
    // start is a planned restart.
    let startup = restart::startup(layout, &hub);
    let components = Arc::new(Mutex::new(components::Components::with_hub(
        instance.clone(),
        std::env::current_dir()?,
        Arc::clone(&hub),
    )));
    let registry = instance_delegations(layout);
    // Descendants a delegated agent leaves behind reparent to the daemon, not init.
    if !delegations::become_child_subreaper() {
        tracing::debug!("SCV daemon is not a child subreaper on this platform");
    }
    let cancellation = CancellationToken::new();
    let restarter = restart::Restarter::new(
        instance.clone(),
        hub,
        Arc::clone(&registry),
        &components,
        cancellation.clone(),
    );
    components
        .lock()
        .await
        .set_restarter(Arc::clone(&restarter));
    let notices = tokio::spawn(restart::announce(
        layout.clone(),
        startup,
        restarter.notifier().clone(),
        cancellation.clone(),
    ));
    let _notices_abort = AbortGuard(notices.abort_handle());
    let monitor = tokio::spawn(restart::monitor(
        restarter.notifier().clone(),
        cancellation.clone(),
    ));
    let _monitor_abort = AbortGuard(monitor.abort_handle());
    let delegation_registry = Arc::clone(&registry);
    let delegation_cancel = cancellation.clone();
    let mut delegation_task = tokio::spawn(async move {
        // The first tick is immediate: orphans from before a restart go first.
        let mut interval = tokio::time::interval(DELEGATION_RECONCILE_INTERVAL);
        loop {
            tokio::select! {
                biased;
                () = delegation_cancel.cancelled() => break,
                _ = interval.tick() => {
                    reconcile_delegations(Arc::clone(&delegation_registry)).await;
                    let zombies = delegations::reap_orphaned_zombies();
                    if zombies > 0 {
                        tracing::debug!("Reaped {zombies} exited orphan processes");
                    }
                }
            }
        }
    });
    let _delegation_abort = AbortGuard(delegation_task.abort_handle());
    let tasks = TaskTracker::new();
    let mut clients = tokio::task::JoinSet::new();
    let refresh_components = components.clone();
    let refresh_cancel = cancellation.clone();
    let mut refresh_task = tokio::spawn(async move {
        let mut refresh = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                biased;
                () = refresh_cancel.cancelled() => break,
                _ = refresh.tick() => {
                    tokio::select! {
                        biased;
                        () = refresh_cancel.cancelled() => break,
                        result = async { refresh_components.lock().await.reconcile().await } => {
                            if result.is_err() { tracing::warn!("Component account discovery failed"); }
                        }
                    }
                }
            }
        }
    });
    let _refresh_abort = AbortGuard(refresh_task.abort_handle());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                let instance = instance.clone();
                let components = components.clone();
                let registry = Arc::clone(&registry);
                let cancellation = cancellation.clone();
                let tasks = tasks.clone();
                clients.spawn(async move {
                    let (reader, writer) = stream.into_split();
                    if let Err(error) = run_managed(reader, writer, instance, Some(components), registry, cancellation, tasks).await {
                        tracing::warn!(error = format!("{error:#}"), "SCV socket client stopped");
                    }
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {},
            _ = tokio::signal::ctrl_c() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
        }
    };
    drop(listener);
    cancellation.cancel();
    let _ = (&mut refresh_task).await;
    let _ = (&mut delegation_task).await;
    components.lock().await.shutdown().await;
    if tokio::time::timeout(Duration::from_secs(8), async {
        while clients.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        clients.abort_all();
        while clients.join_next().await.is_some() {}
    }
    tasks.close();
    tasks.wait().await;
    let _ = tokio::fs::remove_file(path).await;
    restart::clean_shutdown(layout);
    result
}

pub(crate) const DELEGATION_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// The delegation registry for this process's SCV instance.
pub(crate) fn instance_delegations(layout: &Layout) -> Arc<DelegationRegistry> {
    Arc::new(DelegationRegistry::new(layout))
}

/// Stop orphaned delegations of this instance and log what was stopped.
pub(crate) async fn reconcile_delegations(registry: Arc<DelegationRegistry>) {
    let report = registry.reconcile().await;
    if !report.reaped.is_empty() {
        tracing::info!(
            "Reaped {} orphaned delegations: {}",
            report.reaped.len(),
            report.reaped.join(", ")
        );
    }
    if report.removed > 0 {
        tracing::debug!(
            "Removed {} delegation records whose processes had exited",
            report.removed
        );
    }
    if report.stale_markers > 0 {
        tracing::debug!(
            "Removed {} conversation markers whose SCV process had exited",
            report.stale_markers
        );
    }
}

/// A persistent advisory lock closes the stale-socket unlink/bind race.
pub(crate) struct SocketLock(std::fs::File);
impl SocketLock {
    pub(crate) fn acquire(socket: &Path) -> Result<Self> {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(socket.with_extension("lock"))?;
        // SAFETY: flock operates on this owned, live file descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(anyhow!("SCV daemon already owns this socket"));
        }
        Ok(Self(file))
    }
}
impl Drop for SocketLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: the descriptor remains live until this drop returns.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Aborts a task when dropped, so a task the daemon spawned never outlives it.
pub(crate) struct AbortGuard(pub(crate) tokio::task::AbortHandle);
impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use protocol::{
    AppId, DeploymentId, DesiredInstanceState, GuestPath, HranaPipelineRequest, InstanceState,
    SqliteOperation, SqliteOutcome, SqliteQuery, SqliteSessionId,
};
use tokio::sync::{mpsc, oneshot, watch};

use super::client::{GuestSqlite, GuestSqliteError};
use crate::domain::deployment_tasks::DeploymentTaskLease;
use crate::domain::filesystem::reader::guest_vsock_path;
use crate::host::Host;
use crate::state::GuestActivity;

const MAX_HOST_SESSIONS: usize = 128;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn failed(code: &str, message: &str) -> SqliteOutcome {
    SqliteOutcome::Failed {
        code: code.into(),
        message: message.into(),
    }
}

fn expired() -> SqliteOutcome {
    failed(
        "SQLITE_ABORT",
        "The database session is no longer available for this deployment.",
    )
}

struct Opening {
    app_id: AppId,
    deployment_id: DeploymentId,
    id: SqliteSessionId,
    root: GuestPath,
    path: GuestPath,
}

struct Worker {
    host: Arc<Host>,
    registration: Registration,
    lease: DeploymentTaskLease,
    closing: watch::Receiver<bool>,
    requests: mpsc::Receiver<Pipeline>,
    root: GuestPath,
    path: GuestPath,
}

struct Pipeline {
    body: HranaPipelineRequest,
    reply: oneshot::Sender<SqliteOutcome>,
}

struct Entry {
    app_id: AppId,
    deployment_id: DeploymentId,
    requests: mpsc::Sender<Pipeline>,
    close: watch::Sender<bool>,
    completed: watch::Receiver<bool>,
}

#[derive(Default)]
pub struct SqliteSessions {
    entries: Mutex<BTreeMap<SqliteSessionId, Arc<Entry>>>,
}

struct Registration {
    sessions: Arc<SqliteSessions>,
    id: SqliteSessionId,
    entry: Arc<Entry>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut entries = lock(&self.sessions.entries);
        if entries
            .get(&self.id)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
        {
            entries.remove(&self.id);
        }
    }
}

struct Completion(watch::Sender<bool>);

impl Drop for Completion {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

async fn signalled(receiver: &mut watch::Receiver<bool>) {
    let _ = receiver.wait_for(|value| *value).await;
}

impl SqliteSessions {
    pub async fn query(self: &Arc<Self>, host: &Arc<Host>, query: SqliteQuery) -> SqliteOutcome {
        let SqliteQuery {
            app_id,
            deployment_id,
            session_id,
            operation,
        } = query;
        if let SqliteOperation::Open { root, path } = operation {
            return self
                .open(
                    host,
                    Opening {
                        app_id,
                        deployment_id,
                        id: session_id,
                        root,
                        path,
                    },
                )
                .await;
        }
        let entry = lock(&self.entries).get(&session_id).cloned();
        let Some(entry) = entry else {
            return if matches!(operation, SqliteOperation::Close) {
                SqliteOutcome::Closed
            } else {
                expired()
            };
        };
        if entry.app_id != app_id || entry.deployment_id != deployment_id {
            return expired();
        }
        match operation {
            SqliteOperation::Close => {
                entry.close.send_replace(true);
                let mut completed = entry.completed.clone();
                if tokio::time::timeout(CLOSE_TIMEOUT, signalled(&mut completed))
                    .await
                    .is_err()
                {
                    return expired();
                }
                SqliteOutcome::Closed
            }
            SqliteOperation::Pipeline { body } => {
                let (reply, received) = oneshot::channel();
                if entry.requests.try_send(Pipeline { body, reply }).is_err() {
                    return failed(
                        "SQLITE_BUSY",
                        "The database session already has a pending request.",
                    );
                }
                received.await.unwrap_or_else(|_| expired())
            }
            SqliteOperation::Open { .. } => expired(),
        }
    }

    async fn open(self: &Arc<Self>, host: &Arc<Host>, opening: Opening) -> SqliteOutcome {
        let Opening {
            app_id,
            deployment_id,
            id,
            root,
            path,
        } = opening;
        let Some(lease) = host.deployment_tasks.start(&app_id, &deployment_id) else {
            return expired();
        };
        let (requests, pending) = mpsc::channel(1);
        let (close, closing) = watch::channel(false);
        let (completed, completion) = watch::channel(false);
        let entry = Arc::new(Entry {
            app_id,
            deployment_id,
            requests,
            close,
            completed: completion,
        });
        {
            let mut entries = lock(&self.entries);
            if entries.contains_key(&id) {
                return failed(
                    "SQLITE_MISUSE",
                    "The database session identifier is already in use.",
                );
            }
            if entries.len() >= MAX_HOST_SESSIONS
                || entries
                    .values()
                    .filter(|held| held.app_id == entry.app_id)
                    .count()
                    >= guest_contract::sqlite::MAX_SESSIONS_PER_GUEST
            {
                return failed("SQLITE_BUSY", "No database session is available.");
            }
            entries.insert(id.clone(), entry.clone());
        }
        let registration = Registration {
            sessions: self.clone(),
            id,
            entry,
        };
        let (reply, received) = oneshot::channel();
        let host = host.clone();
        tokio::spawn(async move {
            let _completion = Completion(completed);
            Worker {
                host,
                registration,
                lease,
                closing,
                requests: pending,
                root,
                path,
            }
            .run(reply)
            .await;
        });
        received.await.unwrap_or_else(|_| expired())
    }
}

async fn current(host: &Host, entry: &Entry) -> bool {
    let named = host.cache.lock().await.latest().is_some_and(|desired| {
        desired.instances.iter().any(|instance| {
            instance.app_id == entry.app_id
                && instance.deployment_id == entry.deployment_id
                && instance.desired_state != DesiredInstanceState::Stopped
        })
    });
    named && host.state.snapshot().await.isolated
}

async fn prepare(
    host: &Host,
    entry: &Entry,
    root: &GuestPath,
    path: &GuestPath,
) -> Result<(GuestSqlite, GuestActivity), SqliteOutcome> {
    let (activity, needs_wake) = {
        let _transition = host.state.transition(&entry.app_id).await;
        if !current(host, entry).await {
            return Err(expired());
        }
        let record = host.state.record(&entry.app_id).await.ok_or_else(expired)?;
        if record.deployment_id != entry.deployment_id
            || !record.desired_running
            || (record.stop_requested && record.state != InstanceState::Idle)
            || !matches!(
                record.state,
                InstanceState::Idle
                    | InstanceState::Running
                    | InstanceState::Starting
                    | InstanceState::Unhealthy
            )
        {
            return Err(expired());
        }
        (
            host.state.guest_activity(&entry.app_id),
            record.state == InstanceState::Idle,
        )
    };
    host.state
        .mark_active(&entry.app_id, crate::clock::now_ms())
        .await;
    if needs_wake {
        host.waker.wake(&entry.app_id).await.map_err(|_| {
            failed(
                "SQLITE_CANTOPEN",
                "The app could not be woken for a database query.",
            )
        })?;
    }
    {
        let _transition = host.state.transition(&entry.app_id).await;
        if !current(host, entry).await
            || !host.state.record(&entry.app_id).await.is_some_and(|record| {
                record.deployment_id == entry.deployment_id
                    && record.desired_running
                    && !record.stop_requested
                    && matches!(
                        record.state,
                        InstanceState::Running | InstanceState::Starting | InstanceState::Unhealthy
                    )
            })
        {
            return Err(expired());
        }
    }
    let client = GuestSqlite::connect(&guest_vsock_path(host, &entry.app_id), root, path)
        .await
        .map_err(refused)?;
    Ok((client, activity))
}

fn refused(error: GuestSqliteError) -> SqliteOutcome {
    match error {
        GuestSqliteError::Refused { code, message } => SqliteOutcome::Failed { code, message },
        GuestSqliteError::Unavailable => failed("SQLITE_IOERR", "The guest database service is unavailable."),
        GuestSqliteError::Malformed => failed(
            "SQLITE_PROTOCOL",
            "The guest database service returned an invalid response.",
        ),
    }
}

impl Worker {
    async fn run(self, mut opened: oneshot::Sender<SqliteOutcome>) {
        let Worker {
            host,
            registration,
            lease,
            mut closing,
            mut requests,
            root,
            path,
        } = self;
        let entry = &registration.entry;
        let mut cancellation = lease.cancellation();
        let _lease = lease;
        let prepared = tokio::select! {
            biased;
            _ = signalled(&mut cancellation) => return,
            _ = signalled(&mut closing) => return,
            _ = opened.closed() => return,
            result = prepare(&host, entry, &root, &path) => result,
        };
        let (mut client, _activity) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = opened.send(error);
                return;
            }
        };
        if opened.send(SqliteOutcome::Opened).is_err() {
            return;
        }
        loop {
            let next = tokio::select! {
                biased;
                _ = signalled(&mut cancellation) => return,
                _ = signalled(&mut closing) => return,
                _ = tokio::time::sleep(guest_contract::sqlite::SESSION_IDLE_TIMEOUT) => return,
                next = requests.recv() => next,
            };
            let Some(Pipeline { body, mut reply }) = next else {
                return;
            };
            let outcome = tokio::select! {
                biased;
                _ = signalled(&mut cancellation) => return,
                _ = signalled(&mut closing) => return,
                _ = reply.closed() => return,
                result = async {
                    if !current(&host, entry).await { return Err(expired()); }
                    client.pipeline(&body).await.map_err(refused)
                } => result,
            };
            match outcome {
                Ok(body) => {
                    host.state
                        .mark_active(&entry.app_id, crate::clock::now_ms())
                        .await;
                    let closed = body.baton.is_none();
                    if reply.send(SqliteOutcome::Responded { body }).is_err() || closed {
                        return;
                    }
                }
                Err(error) => {
                    let _ = reply.send(error);
                    return;
                }
            }
        }
    }
}

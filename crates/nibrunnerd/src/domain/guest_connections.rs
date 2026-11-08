use std::time::Duration;

use protocol::{AppId, DeploymentId, DesiredInstanceState, InstanceState};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::domain::filesystem::reader::guest_vsock_path;
use crate::domain::guest_line;
use crate::host::Host;
use crate::state::GuestActivity;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum GuestConnectionError {
    #[error("the app no longer runs the requested deployment on this host")]
    DeploymentChanged,
    #[error("the app is not available for a guest service connection")]
    Unavailable,
    #[error("the app could not be woken for a guest service connection")]
    WakeFailed,
    #[error("the guest service could not be reached")]
    Unreachable,
    #[error("the guest service connection timed out")]
    TimedOut,
}

pub struct GuestConnection {
    wire: BufReader<UnixStream>,
    _activity: GuestActivity,
}

impl GuestConnection {
    pub fn stream(&mut self) -> &mut (impl AsyncRead + AsyncWrite + Unpin) {
        &mut self.wire
    }
}

async fn current(
    host: &Host,
    app_id: &AppId,
    deployment_id: &DeploymentId,
) -> Result<bool, GuestConnectionError> {
    let wanted = host.cache.lock().await.latest().is_some_and(|desired| {
        desired.instances.iter().any(|instance| {
            instance.app_id == *app_id
                && instance.deployment_id == *deployment_id
                && instance.desired_state != DesiredInstanceState::Stopped
        })
    });
    let record = host
        .state
        .record(app_id)
        .await
        .ok_or(GuestConnectionError::DeploymentChanged)?;
    if !wanted || record.deployment_id != *deployment_id {
        return Err(GuestConnectionError::DeploymentChanged);
    }
    if !host.state.snapshot().await.isolated
        || !record.desired_running
        || (record.stop_requested && !record.is_idle())
        || !matches!(
            record.state,
            InstanceState::Idle | InstanceState::Running | InstanceState::Starting | InstanceState::Unhealthy
        )
    {
        return Err(GuestConnectionError::Unavailable);
    }
    Ok(record.is_idle())
}

async fn dial(
    host: &Host,
    app_id: &AppId,
    deployment_id: &DeploymentId,
    port: u32,
) -> Result<GuestConnection, GuestConnectionError> {
    let (activity, idle) = {
        let _transition = host.state.transition(app_id).await;
        let idle = current(host, app_id, deployment_id).await?;
        let activity = host.state.guest_activity(app_id);
        host.state.mark_active(app_id, crate::clock::now_ms()).await;
        (activity, idle)
    };
    if idle {
        host.waker
            .wake(app_id)
            .await
            .map_err(|_| GuestConnectionError::WakeFailed)?;
    }
    let path = {
        let _transition = host.state.transition(app_id).await;
        if current(host, app_id, deployment_id).await? {
            return Err(GuestConnectionError::Unavailable);
        }
        guest_vsock_path(host, app_id)
    };
    let stream = crate::unix_socket::connect(&path)
        .await
        .map_err(|_| GuestConnectionError::Unreachable)?;
    let mut wire = BufReader::new(stream);
    wire.write_all(guest_contract::vsock::connect_request(port).as_bytes())
        .await
        .map_err(|_| GuestConnectionError::Unreachable)?;
    let reply = guest_line::read(&mut wire, CONNECT_TIMEOUT)
        .await
        .ok_or(GuestConnectionError::TimedOut)?;
    guest_contract::vsock::read_connect_reply(&reply, port).map_err(|_| GuestConnectionError::Unreachable)?;
    {
        let _transition = host.state.transition(app_id).await;
        current(host, app_id, deployment_id).await?;
    }
    Ok(GuestConnection {
        wire,
        _activity: activity,
    })
}

pub async fn connect(
    host: &Host,
    app_id: &AppId,
    deployment_id: &DeploymentId,
    port: u32,
) -> Result<GuestConnection, GuestConnectionError> {
    tokio::time::timeout(CONNECT_TIMEOUT, dial(host, app_id, deployment_id, port))
        .await
        .map_err(|_| GuestConnectionError::TimedOut)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::guest_connections::host as fixture;
    use crate::test_support::*;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    const PORT: u32 = 51099;

    struct WakeHeld(crate::state::SharedState);

    #[async_trait::async_trait]
    impl crate::ports::Waker for WakeHeld {
        async fn wake(&self, app_id: &AppId) -> Result<(), crate::ports::WakeRefusal> {
            assert!(self.0.has_guest_activity(app_id));
            self.0
                .modify(|snapshot| {
                    let record = snapshot.records.get_mut(app_id).unwrap();
                    record.state = InstanceState::Running;
                    record.stop_requested = false;
                })
                .await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_service_stream_retains_buffered_bytes_and_holds_the_app_active_until_dropped() {
        let (host, _directory, listener) = fixture().await;
        let guest = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = BufReader::new(stream);
            let mut line = String::new();
            wire.read_line(&mut line).await.unwrap();
            assert_eq!(line, guest_contract::vsock::connect_request(PORT));
            wire.write_all(b"OK 1234\nhello").await.unwrap();
            let mut request = [0; 3];
            wire.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"bye");
        });
        let mut connection = connect(host.arc(), &app_id(), &deployment_id(), PORT)
            .await
            .unwrap();
        assert!(host.state.has_guest_activity(&app_id()));
        let mut message = [0; 5];
        connection.stream().read_exact(&mut message).await.unwrap();
        assert_eq!(&message, b"hello");
        connection.stream().write_all(b"bye").await.unwrap();
        drop(connection);
        assert!(!host.state.has_guest_activity(&app_id()));
        guest.await.unwrap();
    }

    #[tokio::test]
    async fn another_deployment_cannot_open_a_connection_or_hold_activity() {
        let (host, _directory, _listener) = fixture().await;
        let other = DeploymentId::parse("another-deployment").unwrap();
        assert!(matches!(
            connect(host.arc(), &app_id(), &other, PORT).await,
            Err(GuestConnectionError::DeploymentChanged)
        ));
        assert!(!host.state.has_guest_activity(&app_id()));
    }

    #[tokio::test]
    async fn a_sleeping_app_is_held_before_waking_and_kept_up_until_the_connection_closes() {
        let (mut host, _directory, listener) = fixture().await;
        host.cache.lock().await.accept(desired_state(|desired| {
            desired.instances = vec![desired_instance(|instance| {
                instance.desired_state = DesiredInstanceState::OnRequest;
            })];
        }));
        host.state
            .modify(|snapshot| {
                let record = snapshot.records.get_mut(&app_id()).unwrap();
                record.state = InstanceState::Idle;
                record.stop_requested = true;
            })
            .await;
        Arc::get_mut(&mut host.host).unwrap().waker = Arc::new(WakeHeld(host.state.clone()));
        let guest = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = BufReader::new(stream);
            wire.read_line(&mut String::new()).await.unwrap();
            wire.write_all(b"OK 1234\n").await.unwrap();
        });
        let connection = connect(host.arc(), &app_id(), &deployment_id(), PORT)
            .await
            .unwrap();
        assert!(host.state.has_guest_activity(&app_id()));
        assert_eq!(
            host.state.record(&app_id()).await.unwrap().state,
            InstanceState::Running
        );
        drop(connection);
        assert!(!host.state.has_guest_activity(&app_id()));
        guest.await.unwrap();
    }

    #[tokio::test]
    async fn a_deployment_replaced_during_the_handshake_is_refused() {
        let (host, _directory, listener) = fixture().await;
        let state = host.arc().clone();
        let guest = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = BufReader::new(stream);
            wire.read_line(&mut String::new()).await.unwrap();
            state.cache.lock().await.accept(desired_state(|desired| {
                desired.instances = vec![desired_instance(|instance| {
                    instance.deployment_id = DeploymentId::parse("replacement").unwrap();
                })];
            }));
            wire.write_all(b"OK 1234\n").await.unwrap();
        });
        assert!(matches!(
            connect(host.arc(), &app_id(), &deployment_id(), PORT).await,
            Err(GuestConnectionError::DeploymentChanged)
        ));
        assert!(!host.state.has_guest_activity(&app_id()));
        guest.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_pending_connection_releases_its_activity() {
        let (host, _directory, listener) = fixture().await;
        let (ready, observed) = tokio::sync::oneshot::channel();
        let guest = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = BufReader::new(stream);
            wire.read_line(&mut String::new()).await.unwrap();
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let state = host.arc().clone();
        let connecting =
            tokio::spawn(async move { connect(&state, &app_id(), &deployment_id(), PORT).await });
        tokio::time::timeout(Duration::from_secs(5), observed)
            .await
            .unwrap()
            .unwrap();
        assert!(host.state.has_guest_activity(&app_id()));
        connecting.abort();
        let _ = connecting.await;
        assert!(!host.state.has_guest_activity(&app_id()));
        guest.abort();
    }
}

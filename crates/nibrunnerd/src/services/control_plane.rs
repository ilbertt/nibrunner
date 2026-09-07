//! Holding a session with the control plane, and answering what it asks.
//!
//! Ported from `apps/agent/src/services/agent-session-holder.service.ts` and
//! `apps/agent/src/lib/agent/filesystem.ts`.
//!
//! The control plane is an addon rather than a second input. What it fetches, it writes into the
//! file the reconciler already watches, so the reconciler still has exactly one source and cannot
//! learn which of them produced the document it converged on. What it *asks* — a read of a
//! tenant's files — is answered without touching the host at all.

use std::sync::Arc;

use protocol::{
    AgentSessionRequest, FilesystemQuery, FilesystemQueryResponse, FilesystemQueryResult, HostCapacity,
    SecretString,
};
use tokio::sync::Mutex;

use crate::adapters::control_plane::{ControlPlaneClient, ControlPlaneError};
use crate::host::Host;
use crate::services::report::capacity::{read_filesystem_space, read_vcpu_count};

/// The session this host is registered under, opened lazily and re-opened when the control plane
/// stops accepting it.
///
/// Lazily because a host whose control plane is down is a host that still runs what it was last
/// told to run: refusing to start without a session would take a working host offline over a
/// connection that has nothing to do with the tenants on it.
pub struct SessionHolder {
    client: ControlPlaneClient,
    held: Mutex<Option<SecretString>>,
}

impl SessionHolder {
    pub fn new(client: ControlPlaneClient) -> Self {
        Self {
            client,
            held: Mutex::new(None),
        }
    }

    pub fn client(&self) -> &ControlPlaneClient {
        &self.client
    }

    /// The token to use, opening a session first where there is none.
    pub async fn current(&self, host: &Host) -> Result<SecretString, ControlPlaneError> {
        let mut held = self.held.lock().await;
        if let Some(token) = held.as_ref() {
            return Ok(token.clone());
        }
        let space = read_filesystem_space(&host.config.state_dir).unwrap_or_default();
        let session = self
            .client
            .open_session(&AgentSessionRequest {
                // Sent where this host already has one, so a reinstalled host rejoins as the same
                // host rather than as a new one.
                host_id: host.known_host_id().await,
                versions: crate::run::host_versions(host),
                capacity: HostCapacity {
                    vcpu_count: read_vcpu_count(),
                    memory_mib: host.guest_memory_mib,
                    cache_bytes: space.total_bytes,
                },
            })
            .await?;
        // Written once and never overwritten: the control plane assigns an id on the first
        // registration and every session after it is the same host coming back.
        host.remember_host_id(session.host_id.as_str()).await;
        tracing::info!(host_id = %session.host_id, "a session with the control plane is open");
        *held = Some(session.session_token.clone());
        Ok(session.session_token)
    }

    /// Dropped so the next call opens a new one. Called where the control plane said the token is
    /// no longer good, which is a thing that happens on its schedule and not on this host's.
    pub async fn expired(&self) {
        self.held.lock().await.take();
    }

    /// Whatever went wrong, and whether it means the session has to be reopened.
    pub async fn note(&self, error: &ControlPlaneError) {
        if error.is_session_expired() {
            self.expired().await;
        }
    }
}

/// A read this host can serve is one it holds a slot for, which is what the slot table records.
///
/// Sent on every poll rather than registered once: an app torn down between two polls stops being
/// offered on the next one, with nothing to invalidate.
pub async fn served_app_ids(host: &Host) -> Vec<protocol::AppId> {
    crate::services::filesystem::reader::served_app_ids(host).await
}

/// One poll: ask for a read, and answer it if one came back.
///
/// Answered whatever happens, because the failure is the answer as far as whoever asked is
/// concerned. A host that stays quiet about a guest it could not read turns a refusal somebody
/// could act on into a timeout they cannot.
pub async fn answer_one_query(
    host: &Host,
    sessions: &Arc<SessionHolder>,
) -> Result<Option<FilesystemQuery>, ControlPlaneError> {
    let token = sessions.current(host).await?;
    let response = sessions
        .client()
        .fetch_filesystem_query(&token, served_app_ids(host).await)
        .await?;
    let FilesystemQueryResponse::Query { query } = response else {
        return Ok(None);
    };
    let result: FilesystemQueryResult = crate::services::filesystem::reader::answer(host, &query).await;
    sessions
        .client()
        .send_filesystem_query_result(&token, &result)
        .await?;
    Ok(Some(query))
}

/// One poll of the document. `true` where it moved, which is what the caller logs.
pub async fn poll_desired_state(
    host: &Host,
    sessions: &Arc<SessionHolder>,
) -> Result<bool, ControlPlaneError> {
    let token = sessions.current(host).await?;
    crate::adapters::control_plane::poll_once(sessions.client(), &host.config.desired_state_file, &token)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// A host whose control plane is unreachable still runs what it was last told to run, so the
    /// failure has to be something a loop can back off on rather than something that takes the
    /// host down with it.
    #[tokio::test]
    async fn a_control_plane_that_cannot_be_reached_is_an_error_and_not_a_panic() {
        let host = test_host().await;
        let sessions = Arc::new(SessionHolder::new(ControlPlaneClient::new("http://127.0.0.1:1")));
        assert!(answer_one_query(host.arc(), &sessions).await.is_err());
    }

    /// An app torn down between two polls stops being offered on the next one, with nothing to
    /// invalidate.
    #[tokio::test]
    async fn only_the_apps_this_host_holds_a_slot_for_are_offered() {
        let host = test_host().await;
        assert!(served_app_ids(host.arc()).await.is_empty());
        host.slot_for(&app_id()).await.unwrap();
        assert_eq!(served_app_ids(host.arc()).await, vec![app_id()]);
    }
}

//! Opaque relay: pairs the two authorized peers of a relay session and
//! forwards their protocol frames without inspecting them.
//!
//! Only bounded in-memory buffering is used ([`CHANNEL_DEPTH`] messages per
//! direction); payloads are never written to disk or the database.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::db::PgPool;

/// How long the first peer of a session waits for its counterpart.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded per-direction buffer between a socket and its forwarding task.
pub const CHANNEL_DEPTH: usize = 32;

/// Tracks which relay sessions have a peer waiting for its counterpart.
#[derive(Default)]
pub struct RelayRegistry {
    sessions: Mutex<HashMap<Uuid, WaitingJoiner>>,
}

struct WaitingJoiner {
    sender: mpsc::Sender<Message>,
    notify: oneshot::Sender<mpsc::Sender<Message>>,
    token: Uuid,
}

/// The outcome of attempting to join a relay session.
pub enum JoinOutcome {
    /// No counterpart yet; wait on `receiver` (cancellable via `cancel`).
    Waiting {
        receiver: oneshot::Receiver<mpsc::Sender<Message>>,
        token: Uuid,
    },
    /// The counterpart was already waiting; forward to the returned channel.
    Paired(mpsc::Sender<Message>),
}

impl RelayRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `my_sender` for `session_id`.
    ///
    /// Exactly one of the two concurrent callers gets `Paired`; the other
    /// gets `Waiting`. A third caller also gets `Waiting` and will time out,
    /// because the first waiter's entry is consumed by the pairing call.
    pub fn join(&self, session_id: Uuid, my_sender: mpsc::Sender<Message>) -> JoinOutcome {
        let mut sessions = self.sessions.lock().expect("relay registry poisoned");
        match sessions.remove(&session_id) {
            Some(waiting) => {
                let _ = waiting.notify.send(my_sender);
                JoinOutcome::Paired(waiting.sender)
            }
            None => {
                let (notify, receiver) = oneshot::channel();
                let token = Uuid::new_v4();
                sessions.insert(
                    session_id,
                    WaitingJoiner {
                        sender: my_sender,
                        notify,
                        token,
                    },
                );
                JoinOutcome::Waiting { receiver, token }
            }
        }
    }

    /// Removes the waiting entry created by the join that returned `token`.
    pub fn cancel(&self, session_id: Uuid, token: Uuid) {
        let mut sessions = self.sessions.lock().expect("relay registry poisoned");
        let is_ours = sessions
            .get(&session_id)
            .map(|waiting| waiting.token == token)
            .unwrap_or(false);
        if is_ours {
            sessions.remove(&session_id);
        }
    }
}

/// Runs one side of a relayed session until either peer disconnects.
pub async fn run(
    pool: PgPool,
    registry: Arc<RelayRegistry>,
    session_id: Uuid,
    device_id: Uuid,
    socket: WebSocket,
) {
    let (my_sender, mut my_receiver) = mpsc::channel::<Message>(CHANNEL_DEPTH);
    let peer_sender = match registry.join(session_id, my_sender) {
        JoinOutcome::Paired(peer_sender) => peer_sender,
        JoinOutcome::Waiting { receiver, token } => {
            match tokio::time::timeout(JOIN_TIMEOUT, receiver).await {
                Ok(Ok(peer_sender)) => peer_sender,
                _ => {
                    registry.cancel(session_id, token);
                    tracing::info!(%session_id, %device_id, "relay join timed out");
                    close_session(&pool, session_id).await;
                    return;
                }
            }
        }
    };

    mark_active(&pool, session_id).await;
    tracing::info!(%session_id, %device_id, "relay session paired");

    let (mut sink, mut stream) = socket.split();

    let mut outbound = tokio::spawn(async move {
        while let Some(message) = my_receiver.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut inbound = tokio::spawn(async move {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Text(_) | Message::Binary(_) => {
                    if peer_sender.send(message).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    });

    tokio::select! {
        _ = &mut outbound => {}
        _ = &mut inbound => {}
    }
    outbound.abort();
    inbound.abort();

    close_session(&pool, session_id).await;
    tracing::info!(%session_id, %device_id, "relay session ended");
}

async fn close_session(pool: &PgPool, session_id: Uuid) {
    cleanup_session(pool, session_id).await;
}

/// Marks an open or active session closed (public: also used when a session
/// is found expired at join time).
pub async fn cleanup_session(pool: &PgPool, session_id: Uuid) {
    if let Ok(client) = pool.get().await {
        client
            .execute(
                "UPDATE relay_sessions SET status = 'closed' \
                 WHERE session_id = $1 AND status IN ('open', 'active')",
                &[&session_id],
            )
            .await
            .ok();
    }
}

/// Closes expired open/active sessions and returns how many rows changed.
pub async fn cleanup_expired_sessions(pool: &PgPool) -> anyhow::Result<u64> {
    let client = pool.get().await?;
    let rows = client
        .execute(
            "UPDATE relay_sessions SET status = 'closed' \
             WHERE status IN ('open', 'active') AND expires_at < now()",
            &[],
        )
        .await?;
    Ok(rows)
}

/// Flips a session from `open` to `active` when both peers are present.
async fn mark_active(pool: &PgPool, session_id: Uuid) {
    if let Ok(client) = pool.get().await {
        client
            .execute(
                "UPDATE relay_sessions SET status = 'active' \
                 WHERE session_id = $1 AND status = 'open'",
                &[&session_id],
            )
            .await
            .ok();
    }
}

/// Periodically runs [`cleanup_expired_sessions`] until cancelled.
pub async fn cleanup_loop(pool: PgPool, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match cleanup_expired_sessions(&pool).await {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, "closed expired relay sessions"),
            Err(error) => tracing::warn!(%error, "relay session cleanup failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> (mpsc::Sender<Message>, mpsc::Receiver<Message>) {
        mpsc::channel(CHANNEL_DEPTH)
    }

    #[test]
    fn first_join_waits_second_join_pairs() {
        let registry = RelayRegistry::new();
        let session_id = Uuid::new_v4();

        let (first_tx, _first_rx) = channel();
        let mut waiting_receiver = match registry.join(session_id, first_tx.clone()) {
            JoinOutcome::Waiting { receiver, .. } => receiver,
            JoinOutcome::Paired(_) => panic!("first join must wait"),
        };

        let (second_tx, _second_rx) = channel();
        let paired = match registry.join(session_id, second_tx) {
            JoinOutcome::Paired(peer) => peer,
            JoinOutcome::Waiting { .. } => panic!("second join must pair"),
        };

        // The second joiner can reach the first waiter...
        assert!(paired.try_send(Message::Text("hello".into())).is_ok());
        // ...and the first waiter is notified with the second's channel.
        let first_peers_sender = waiting_receiver.try_recv().expect("notified");
        assert!(
            first_peers_sender
                .try_send(Message::Text("hi".into()))
                .is_ok()
        );
        assert!(first_tx.try_send(Message::Text("x".into())).is_ok());
    }

    #[test]
    fn cancelled_waiter_is_replaced_by_the_next_join() {
        let registry = RelayRegistry::new();
        let session_id = Uuid::new_v4();

        let (first_tx, _first_rx) = channel();
        let JoinOutcome::Waiting { token, .. } = registry.join(session_id, first_tx) else {
            panic!("first join must wait");
        };
        registry.cancel(session_id, token);

        let (second_tx, _second_rx) = channel();
        match registry.join(session_id, second_tx) {
            JoinOutcome::Waiting { .. } => {}
            JoinOutcome::Paired(_) => panic!("no waiter remains after cancel"),
        }
    }

    #[test]
    fn stale_cancel_after_pairing_does_not_corrupt_the_registry() {
        let registry = RelayRegistry::new();
        let session_id = Uuid::new_v4();

        let (first_tx, _first_rx) = channel();
        let JoinOutcome::Waiting {
            token: stale_token, ..
        } = registry.join(session_id, first_tx)
        else {
            panic!("first join must wait");
        };
        let (second_tx, _second_rx) = channel();
        let JoinOutcome::Paired(_) = registry.join(session_id, second_tx) else {
            panic!("second join must pair");
        };

        // The first peer finishes and cancels its (already consumed) token.
        registry.cancel(session_id, stale_token);

        let (third_tx, _third_rx) = channel();
        match registry.join(session_id, third_tx) {
            JoinOutcome::Waiting { .. } => {}
            JoinOutcome::Paired(_) => panic!("registry must be empty after pairing"),
        }
    }
}

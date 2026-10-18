//! The sync session driver: one complete synchronization exchange over an
//! established [`PeerChannel`] (SRD §4.1 steps 4–10, §11).
//!
//! Both sides run the same loop symmetrically:
//!
//! 1. Each side asks for the changes the other has for every shared folder
//!    ([`PeerMessage::ChangesRequest`]).
//! 2. Each side answers with its pending changes ([`PeerMessage::Changes`]).
//! 3. For every change that needs content, the receiver asks for the bytes
//!    ([`PeerMessage::ContentRequest`]) and the sender streams
//!    [`PeerMessage::Chunk`]s from its staging store.
//! 4. The receiver verifies, publishes, and acknowledges
//!    ([`PeerMessage::Ack`]); conflicts are preserved as sibling copies
//!    ([`PeerMessage::Conflict`]).
//! 5. When both sides have nothing outstanding they exchange
//!    [`PeerMessage::Done`] and the session ends.
//!
//! Frame sealing happens below this layer ([`PeerChannel`]), so this module
//! only sees decoded messages.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use syncengine_engine::{IncomingFile, RemotePlan, StagingStore, SyncEngine};
use syncengine_protocol::messages::{MAX_FRAME_BYTES, PeerMessage};
use syncengine_protocol::{
    ChangeId, ChangeRecord, ContentHash, DeviceId, EntryId, FolderId, now_rfc3339,
};
use syncengine_storage::{Peer, Store};

use crate::channel::PeerChannel;
use crate::error::TransportError;

/// Tunables for one session.
#[derive(Debug, Clone, Copy)]
pub struct SessionOptions {
    /// Bytes per [`PeerMessage::Chunk`] frame.
    pub chunk_size: usize,
    /// Abort the session when no frame arrives within this window.
    pub max_idle: Duration,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            chunk_size: 256 * 1024,
            max_idle: Duration::from_secs(120),
        }
    }
}

/// What one session accomplished.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionReport {
    pub peer: Option<DeviceId>,
    /// Shared folders inspected.
    pub folders: usize,
    /// Local changes the peer acknowledged as durably applied.
    pub delivered: usize,
    /// Remote changes applied locally (files and deletions).
    pub applied: usize,
    /// Remote changes preserved as conflict copies.
    pub conflicts: usize,
    /// Application-level error frames seen during the session.
    pub errors: usize,
}

enum IncomingKind {
    File,
    Conflict,
}

struct IncomingTransfer<'a> {
    change: ChangeRecord,
    kind: IncomingKind,
    file: IncomingFile<'a>,
}

/// Runs one full synchronization session with a verified peer.
pub async fn run_session<S: Store>(
    engine: &mut SyncEngine<S>,
    channel: &mut PeerChannel<'_>,
    peer: &Peer,
    options: &SessionOptions,
) -> Result<SessionReport, TransportError> {
    let chunk_size = options.chunk_size.clamp(4 * 1024, MAX_FRAME_BYTES / 2);

    let mut report = SessionReport {
        peer: Some(peer.device_id),
        ..SessionReport::default()
    };

    // A receive-side staging handle decoupled from the engine borrow, so an
    // active transfer can coexist with engine commits.
    let staging = StagingStore::new(engine.staging().root().to_path_buf())?;

    let common_folders = common_folders(engine, &peer.device_id)?;
    report.folders = common_folders.len();
    for folder_id in &common_folders {
        channel
            .send(&PeerMessage::ChangesRequest {
                folder_id: *folder_id,
            })
            .await?;
    }

    let mut outstanding_requests = common_folders.len() as i64;
    let mut outstanding_acks: i64 = 0;
    let mut outstanding_fetches: i64 = 0;
    let mut incoming: HashMap<ChangeId, IncomingTransfer<'_>> = HashMap::new();
    let mut peer_done = false;
    let mut my_done_sent = false;

    loop {
        let quiet = outstanding_requests == 0 && outstanding_acks <= 0 && outstanding_fetches <= 0;
        if !my_done_sent && quiet {
            channel.send(&PeerMessage::Done).await?;
            my_done_sent = true;
        }
        if peer_done && my_done_sent && incoming.is_empty() && quiet {
            break;
        }

        let message = match tokio::time::timeout(options.max_idle, channel.recv()).await {
            Err(_) => return Err(TransportError::Timeout(options.max_idle)),
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None)) => {
                if peer_done && my_done_sent && quiet {
                    break;
                }
                return Err(TransportError::Closed);
            }
            Ok(Err(error)) => return Err(error),
        };

        match message {
            PeerMessage::ChangesRequest { folder_id } => {
                let pending: Vec<ChangeRecord> = engine
                    .store()
                    .pending_for_peer(&peer.device_id)?
                    .into_iter()
                    .filter(|change| change.folder_id == folder_id)
                    .collect();
                outstanding_acks += pending.len() as i64;
                channel
                    .send(&PeerMessage::Changes {
                        folder_id,
                        changes: pending,
                    })
                    .await?;
            }

            PeerMessage::Changes { folder_id, changes } => {
                outstanding_requests -= 1;
                for change in &changes {
                    if change.folder_id != folder_id {
                        report.errors += 1;
                        send_error(
                            channel,
                            "FOLDER_MISMATCH",
                            "change does not belong to the announced folder",
                        )
                        .await?;
                        continue;
                    }
                    if engine.store().get_folder(&change.folder_id)?.is_none() {
                        report.errors += 1;
                        send_error(channel, "UNKNOWN_FOLDER", "folder is not shared locally")
                            .await?;
                        continue;
                    }
                    match engine.plan_remote_change(change)? {
                        RemotePlan::Stale | RemotePlan::AlreadyApplied => {
                            send_ack(engine, channel, change).await?;
                        }
                        RemotePlan::Delete => {
                            engine.commit_remote_delete(change)?;
                            report.applied += 1;
                            send_ack(engine, channel, change).await?;
                        }
                        RemotePlan::ConflictDelete => {
                            engine.commit_conflict_delete(change)?;
                            report.conflicts += 1;
                            send_ack(engine, channel, change).await?;
                        }
                        plan @ (RemotePlan::ApplyFile | RemotePlan::Conflict) => {
                            if incoming.contains_key(&change.change_id) {
                                continue;
                            }
                            let Some(expected_hash) = change.content_hash.clone() else {
                                report.errors += 1;
                                send_error(channel, "NO_CONTENT", "change carries no content hash")
                                    .await?;
                                continue;
                            };
                            let kind = if plan == RemotePlan::Conflict {
                                IncomingKind::Conflict
                            } else {
                                IncomingKind::File
                            };
                            let file = staging.receive(expected_hash.clone(), change.size)?;
                            let offset = file.written();
                            incoming.insert(
                                change.change_id,
                                IncomingTransfer {
                                    change: change.clone(),
                                    kind,
                                    file,
                                },
                            );
                            outstanding_fetches += 1;
                            channel
                                .send(&PeerMessage::ContentRequest {
                                    change_id: change.change_id,
                                    folder_id: change.folder_id,
                                    entry_id: change.entry_id,
                                    expected_hash,
                                    expected_size: change.size,
                                    offset,
                                })
                                .await?;
                        }
                    }
                }
            }

            PeerMessage::ContentRequest {
                change_id,
                folder_id,
                entry_id,
                expected_hash,
                expected_size,
                offset,
            } => {
                serve_content(
                    engine,
                    channel,
                    &change_id,
                    folder_id,
                    entry_id,
                    &expected_hash,
                    expected_size,
                    offset,
                    chunk_size,
                    &mut report,
                )
                .await?;
            }

            PeerMessage::Chunk {
                change_id,
                offset,
                data,
                last,
            } => {
                let mut failure: Option<(&str, String)> = None;
                if let Some(transfer) = incoming.get_mut(&change_id) {
                    match BASE64.decode(data.as_bytes()) {
                        Ok(bytes) => {
                            if let Err(error) = transfer.file.write_chunk(offset, &bytes) {
                                failure = Some(("BAD_CHUNK", error.to_string()));
                            }
                        }
                        Err(_) => {
                            failure =
                                Some(("BAD_CHUNK", "chunk payload is not valid base64".into()));
                        }
                    }
                } else {
                    failure = Some(("UNEXPECTED_CHUNK", "no transfer is in progress".into()));
                }
                if let Some((code, message)) = failure {
                    report.errors += 1;
                    if incoming.remove(&change_id).is_some() {
                        outstanding_fetches -= 1;
                    }
                    send_error(channel, code, &message).await?;
                    continue;
                }
                if last {
                    let transfer = incoming.remove(&change_id).expect("a transfer is active");
                    let IncomingTransfer { change, kind, file } = transfer;
                    file.finish()?;
                    match kind {
                        IncomingKind::File => {
                            engine.commit_remote_file(&change)?;
                            report.applied += 1;
                            send_ack(engine, channel, &change).await?;
                        }
                        IncomingKind::Conflict => {
                            let conflict_path = engine.preserve_conflict_copy(&change)?;
                            engine.mark_change_applied(&change)?;
                            report.conflicts += 1;
                            channel
                                .send(&PeerMessage::Conflict {
                                    change_id: change.change_id,
                                    folder_id: change.folder_id,
                                    entry_id: change.entry_id,
                                    conflict_path: conflict_path.to_string_lossy().into_owned(),
                                })
                                .await?;
                            send_ack(engine, channel, &change).await?;
                        }
                    }
                    outstanding_fetches -= 1;
                }
            }

            PeerMessage::Ack { change_id, .. } => {
                engine.store_mut().acknowledge_change(
                    &change_id,
                    &peer.device_id,
                    &now_rfc3339(),
                )?;
                outstanding_acks -= 1;
                report.delivered += 1;
            }

            PeerMessage::Conflict {
                change_id,
                conflict_path,
                ..
            } => {
                tracing::info!(
                    change = %change_id,
                    path = %conflict_path,
                    "the peer preserved a conflicting revision"
                );
            }

            PeerMessage::Done => {
                peer_done = true;
            }

            PeerMessage::Error { code, message } => {
                tracing::warn!(
                    code = %code,
                    message = %message,
                    peer = %peer.device_id,
                    "peer reported an application error"
                );
                report.errors += 1;
            }

            PeerMessage::Ping => {
                channel.send(&PeerMessage::Pong).await?;
            }
            PeerMessage::Pong => {}
            PeerMessage::Hello { .. } | PeerMessage::HelloAccepted { .. } => {
                tracing::warn!(
                    peer = %peer.device_id,
                    "ignoring a hello frame inside an established session"
                );
            }
        }
    }

    // SRD §14: tombstones whose deletion every trusted folder peer has
    // acknowledged are retired opportunistically at the end of a session.
    match engine.retire_acknowledged_tombstones() {
        Ok(0) => {}
        Ok(retired) => tracing::debug!(retired, "retired acknowledged tombstones"),
        Err(error) => tracing::warn!(%error, "tombstone retirement failed"),
    }

    Ok(report)
}

fn common_folders<S: Store>(
    engine: &SyncEngine<S>,
    peer_id: &DeviceId,
) -> Result<Vec<FolderId>, TransportError> {
    let mut folders = Vec::new();
    for membership in engine.store().list_peer_folders(peer_id)? {
        if folders.contains(&membership.folder_id) {
            continue;
        }
        let enabled = engine
            .store()
            .get_folder(&membership.folder_id)?
            .map(|folder| folder.enabled)
            .unwrap_or(false);
        if enabled {
            folders.push(membership.folder_id);
        }
    }
    Ok(folders)
}

async fn send_error(
    channel: &mut PeerChannel<'_>,
    code: &str,
    message: &str,
) -> Result<(), TransportError> {
    channel
        .send(&PeerMessage::Error {
            code: code.to_string(),
            message: message.to_string(),
        })
        .await
}

async fn send_ack<S: Store>(
    engine: &SyncEngine<S>,
    channel: &mut PeerChannel<'_>,
    change: &ChangeRecord,
) -> Result<(), TransportError> {
    let version = engine
        .store()
        .get_entry(&change.folder_id, &change.relative_path)?
        .map(|entry| entry.version)
        .unwrap_or_else(|| change.version.clone());
    channel
        .send(&PeerMessage::Ack {
            change_id: change.change_id,
            folder_id: change.folder_id,
            version,
        })
        .await
}

#[allow(clippy::too_many_arguments)]
async fn serve_content<S: Store>(
    engine: &SyncEngine<S>,
    channel: &mut PeerChannel<'_>,
    change_id: &syncengine_protocol::ChangeId,
    folder_id: FolderId,
    entry_id: EntryId,
    expected_hash: &ContentHash,
    expected_size: u64,
    offset: u64,
    chunk_size: usize,
    report: &mut SessionReport,
) -> Result<(), TransportError> {
    let Some(change) = engine.store().get_change(change_id)? else {
        report.errors += 1;
        return send_error(channel, "UNKNOWN_CHANGE", "no such change in the journal").await;
    };
    if change.folder_id != folder_id || change.entry_id != entry_id {
        report.errors += 1;
        return send_error(channel, "CHANGE_MISMATCH", "change metadata does not match").await;
    }
    if change.size != expected_size {
        report.errors += 1;
        return send_error(
            channel,
            "SIZE_MISMATCH",
            "requested size is not the change size",
        )
        .await;
    }
    let Some(recorded_hash) = change.content_hash.clone() else {
        report.errors += 1;
        return send_error(channel, "NO_CONTENT", "change has no content").await;
    };
    if !recorded_hash
        .as_str()
        .eq_ignore_ascii_case(expected_hash.as_str())
    {
        report.errors += 1;
        return send_error(
            channel,
            "HASH_MISMATCH",
            "requested hash is not the change content",
        )
        .await;
    }

    let staged_path = engine.staging().path_for(&recorded_hash);
    let usable = staged_path.is_file()
        && engine
            .staging()
            .verify(&recorded_hash, change.size)
            .unwrap_or(false);
    if !usable {
        report.errors += 1;
        return send_error(
            channel,
            "CONTENT_UNAVAILABLE",
            "the staged revision is missing; run sync again to restage",
        )
        .await;
    }
    if offset > change.size {
        report.errors += 1;
        return send_error(
            channel,
            "BAD_OFFSET",
            "offset is past the end of the content",
        )
        .await;
    }

    let mut file = std::fs::File::open(&staged_path)?;
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut position = offset;
    let mut buffer = vec![0u8; chunk_size.clamp(4, MAX_FRAME_BYTES / 2)];
    loop {
        let read = file.read(&mut buffer)?;
        let last = position + read as u64 >= change.size;
        channel
            .send(&PeerMessage::Chunk {
                change_id: *change_id,
                offset: position,
                data: BASE64.encode(&buffer[..read]),
                last,
            })
            .await?;
        position += read as u64;
        if last {
            break;
        }
        if read == 0 {
            report.errors += 1;
            return send_error(
                channel,
                "CONTENT_SHORT",
                "staged file is shorter than declared",
            )
            .await;
        }
    }
    Ok(())
}

//! Length-prefixed framing for byte-oriented transports.
//!
//! Each frame is a 4-byte big-endian length followed by that many payload
//! bytes. Payloads above
//! [`MAX_FRAME_BYTES`](syncengine_protocol::messages::MAX_FRAME_BYTES) are
//! rejected before allocation.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::TransportError;

use syncengine_protocol::messages::MAX_FRAME_BYTES;

/// Writes one frame (length prefix + payload).
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), TransportError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(payload.len()));
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one frame. Returns `Ok(None)` on a clean end-of-stream before any
/// length byte; a truncated frame is an error.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, TransportError> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(length));
    }
    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::UnexpectedEof => {
                TransportError::Protocol("connection closed mid-frame".into())
            }
            _ => TransportError::Io(error),
        })?;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut client, mut server) = duplex(64 * 1024);
        write_frame(&mut client, b"hello").await.unwrap();
        write_frame(&mut client, b"").await.unwrap();
        assert_eq!(read_frame(&mut server).await.unwrap().unwrap(), b"hello");
        assert_eq!(read_frame(&mut server).await.unwrap().unwrap(), b"");
    }

    #[tokio::test]
    async fn clean_close_yields_none_and_truncation_is_an_error() {
        let (client, mut server) = duplex(64);
        drop(client);
        assert!(read_frame(&mut server).await.unwrap().is_none());

        let (mut client, mut server) = duplex(64);
        client.write_all(&[0, 0, 0, 8]).await.unwrap();
        client.write_all(b"abc").await.unwrap();
        drop(client);
        assert!(matches!(
            read_frame(&mut server).await,
            Err(TransportError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn oversized_lengths_are_rejected_without_allocation() {
        let (mut client, mut server) = duplex(64);
        client.write_all(&(u32::MAX).to_be_bytes()).await.unwrap();
        assert!(matches!(
            read_frame(&mut server).await,
            Err(TransportError::FrameTooLarge(_))
        ));

        let (mut client, server) = duplex(64);
        let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(
            write_frame(&mut client, &oversized).await,
            Err(TransportError::FrameTooLarge(_))
        ));
        drop(server);
    }
}

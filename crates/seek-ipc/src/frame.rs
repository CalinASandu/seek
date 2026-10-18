//! Length-prefixed JSON framing: `u32` little-endian byte length, then UTF-8 JSON.

use std::io;

use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::MAX_FRAME_BYTES;

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let body = serde_json::to_vec(v).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await
}

/// Reads one frame. Returns `Ok(None)` on a clean EOF before the length prefix.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Envelope, Request};

    #[tokio::test]
    async fn roundtrip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let msg = Envelope { id: 9, body: Request::ListDevices };
        write_frame(&mut a, &msg).await.unwrap();
        let got: Envelope<Request> = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn eof_is_none() {
        let (a, mut b) = tokio::io::duplex(64);
        drop(a);
        let got: Option<Envelope<Request>> = read_frame(&mut b).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn oversize_length_is_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        let got: io::Result<Option<Envelope<Request>>> = read_frame(&mut b).await;
        assert!(got.is_err());
    }
}

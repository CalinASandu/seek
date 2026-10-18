//! Linking two PCs with a one-time 6-digit code (SPAKE2), then swapping identities under the derived key.
//!
//! SPAKE2 means an eavesdropper learns nothing that lets them test codes offline, and an active attacker
//! gets one guess per attempt (the code allows three). Messages are `u32 LE length | bytes`; an empty
//! message from the host means "rejected".

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::Rng;
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Error;

pub const CODE_VALID_MS: i64 = 120_000;
pub const CODE_ATTEMPTS: u8 = 3;
const IDENTITY: &[u8] = b"seek-pair-v1";
const MAX_PAIR_MSG: usize = 1 << 20;

/// What each PC tells the other when linking.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PairInfo {
    pub host_id: uuid::Uuid,
    pub name: String,
    /// Base64 of the 32-byte static public key.
    pub public_key: String,
    /// SID of the user who linked on this PC (they count as the same person on both PCs).
    pub user_sid: String,
    /// The other PCs already in this PC's group (the guest links with all of them).
    pub members: Vec<MemberInfo>,
    /// TCP port this PC listens on for sync (the other side knows the IP from the connection).
    #[serde(default)]
    pub port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MemberInfo {
    pub host_id: uuid::Uuid,
    pub name: String,
    pub public_key: String,
    pub addr: Option<String>,
}

/// A code shown on the host PC.
#[derive(Clone, Debug)]
pub struct PairingCode {
    pub code: String,
    pub expires_ms: i64,
    attempts_left: u8,
}

impl PairingCode {
    pub fn new(now_ms: i64) -> PairingCode {
        let n: u32 = rand::thread_rng().gen_range(0..1_000_000);
        PairingCode { code: format!("{n:06}"), expires_ms: now_ms + CODE_VALID_MS, attempts_left: CODE_ATTEMPTS }
    }

    pub fn usable(&self, now_ms: i64) -> bool {
        self.attempts_left > 0 && now_ms < self.expires_ms
    }

    /// Uses up one attempt; `false` when none was left.
    pub fn consume_attempt(&mut self) -> bool {
        if self.attempts_left == 0 {
            return false;
        }
        self.attempts_left -= 1;
        true
    }
}

async fn send<S: AsyncWrite + Unpin>(io: &mut S, b: &[u8]) -> Result<(), Error> {
    io.write_all(&(b.len() as u32).to_le_bytes()).await?;
    io.write_all(b).await?;
    io.flush().await?;
    Ok(())
}

async fn recv<S: AsyncRead + Unpin>(io: &mut S) -> Result<Vec<u8>, Error> {
    let mut len = [0u8; 4];
    io.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_PAIR_MSG {
        return Err(Error::Protocol("pairing message too large".into()));
    }
    let mut b = vec![0u8; n];
    io.read_exact(&mut b).await?;
    Ok(b)
}

/// Runs SPAKE2 on `code`; returns the shared key.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(io: &mut S, code: &str) -> Result<ChaCha20Poly1305, Error> {
    let (spake, outbound) =
        Spake2::<Ed25519Group>::start_symmetric(&Password::new(code.as_bytes()), &Identity::new(IDENTITY));
    send(io, &outbound).await?;
    let inbound = recv(io).await?;
    let key = spake.finish(&inbound).map_err(|e| Error::Crypto(format!("{e:?}")))?;
    Ok(ChaCha20Poly1305::new(Key::from_slice(&key[..32])))
}

fn seal(cipher: &ChaCha20Poly1305, role: u8, info: &PairInfo) -> Result<Vec<u8>, Error> {
    let plain = serde_json::to_vec(info).map_err(|e| Error::Protocol(e.to_string()))?;
    cipher.encrypt(Nonce::from_slice(&[role; 12]), plain.as_slice()).map_err(|_| Error::Crypto("encrypt".into()))
}

fn open(cipher: &ChaCha20Poly1305, role: u8, b: &[u8]) -> Result<PairInfo, Error> {
    let plain = cipher.decrypt(Nonce::from_slice(&[role; 12]), b).map_err(|_| Error::WrongCode)?;
    serde_json::from_slice(&plain).map_err(|e| Error::Protocol(format!("bad pairing info: {e}")))
}

const HOST: u8 = 1;
const GUEST: u8 = 2;

/// The PC showing the code. Returns the guest's info once both proved they know the same code.
pub async fn pair_host<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
    code: &str,
    mine: &PairInfo,
) -> Result<PairInfo, Error> {
    let cipher = exchange(io, code).await?;
    let theirs = match open(&cipher, GUEST, &recv(io).await?) {
        Ok(t) => t,
        Err(e) => {
            let _ = send(io, &[]).await;
            return Err(e);
        }
    };
    send(io, &seal(&cipher, HOST, mine)?).await?;
    Ok(theirs)
}

/// The PC where the code is typed.
pub async fn pair_guest<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
    code: &str,
    mine: &PairInfo,
) -> Result<PairInfo, Error> {
    let cipher = exchange(io, code).await?;
    send(io, &seal(&cipher, GUEST, mine)?).await?;
    let reply = recv(io).await?;
    if reply.is_empty() {
        return Err(Error::WrongCode);
    }
    open(&cipher, HOST, &reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> PairInfo {
        PairInfo {
            host_id: uuid::Uuid::new_v4(),
            name: name.into(),
            public_key: "k".into(),
            user_sid: "S-1".into(),
            members: vec![],
            port: 47321,
        }
    }

    #[tokio::test]
    async fn same_code_exchanges_info() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let (ha, gb) = (info("A"), info("B"));
        let ha2 = ha.clone();
        let h = tokio::spawn(async move { pair_host(&mut a, "123456", &ha2).await });
        let got_host = pair_guest(&mut b, "123456", &gb).await.unwrap();
        assert_eq!(got_host, ha);
        assert_eq!(h.await.unwrap().unwrap(), gb);
    }

    #[tokio::test]
    async fn different_code_fails_on_both_sides() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let h = tokio::spawn(async move { pair_host(&mut a, "123456", &info("A")).await });
        assert!(matches!(pair_guest(&mut b, "654321", &info("B")).await, Err(Error::WrongCode)));
        assert!(matches!(h.await.unwrap(), Err(Error::WrongCode)));
    }

    #[test]
    fn code_rules() {
        let mut c = PairingCode::new(1_000);
        assert_eq!(c.code.len(), 6);
        assert!(c.code.chars().all(|ch| ch.is_ascii_digit()));
        assert!(c.usable(1_000 + 119_000) && !c.usable(1_000 + 121_000));
        assert!(c.consume_attempt() && c.consume_attempt() && c.consume_attempt());
        assert!(!c.consume_attempt() && !c.usable(1_001));
    }
}

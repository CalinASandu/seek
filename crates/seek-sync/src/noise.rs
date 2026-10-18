//! Encrypted, mutually authenticated connection between two linked PCs (Noise IK).
//!
//! The dialer knows the listener's static key; the listener learns the dialer's static key from the first
//! (encrypted) handshake message and accepts it only if `accept_key` says it is a linked peer.
//! On the wire every Noise message is `u16 LE length | ciphertext`. A frame travels as a plaintext stream
//! `u32 LE frame length | frame bytes`, split over as many Noise messages as needed.

use std::sync::Arc;

use snow::{Builder, StatelessTransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

use crate::frame::{Frame, MAX_FRAME};
use crate::Error;

pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const MAX_MSG: usize = 65535;
const TAG: usize = 16;
const MAX_PLAIN: usize = MAX_MSG - TAG;

/// This PC's static key pair.
#[derive(Clone)]
pub struct Keys {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys").field("public", &self.public).finish_non_exhaustive()
    }
}

fn builder() -> Builder<'static> {
    Builder::new(NOISE_PARAMS.parse().expect("valid noise params"))
}

fn crypto(e: snow::Error) -> Error {
    Error::Crypto(e.to_string())
}

impl Keys {
    pub fn generate() -> Keys {
        let kp = builder().generate_keypair().expect("key generation");
        Keys { private: to32(&kp.private), public: to32(&kp.public) }
    }

    /// Rebuilds the public key from a stored private key.
    pub fn from_private(private: [u8; 32]) -> Keys {
        // X25519 public = scalar mult of the base point; snow exposes it via a throwaway handshake-free DH.
        let public = x25519_public(&private);
        Keys { private, public }
    }
}

fn to32(v: &[u8]) -> [u8; 32] {
    let mut a = [0u8; 32];
    a.copy_from_slice(&v[..32]);
    a
}

/// X25519 public key for `private` using snow's default resolver.
fn x25519_public(private: &[u8; 32]) -> [u8; 32] {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let mut dh = DefaultResolver.resolve_dh(&snow::params::DHChoice::Curve25519).expect("x25519");
    dh.set(private);
    to32(dh.pubkey())
}

pub struct SecureReader<R> {
    io: R,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    buf: Vec<u8>,
}

pub struct SecureWriter<W> {
    io: W,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    buf: Vec<u8>,
}

async fn read_msg<R: AsyncRead + Unpin>(io: &mut R, buf: &mut Vec<u8>) -> Result<usize, Error> {
    let mut len = [0u8; 2];
    io.read_exact(&mut len).await?;
    let n = u16::from_le_bytes(len) as usize;
    buf.resize(n, 0);
    io.read_exact(&mut buf[..n]).await?;
    Ok(n)
}

async fn write_msg<W: AsyncWrite + Unpin>(io: &mut W, msg: &[u8]) -> Result<(), Error> {
    io.write_all(&(msg.len() as u16).to_le_bytes()).await?;
    io.write_all(msg).await?;
    Ok(())
}

impl<R: AsyncRead + Unpin> SecureReader<R> {
    /// Next frame from the peer.
    pub async fn recv(&mut self) -> Result<Frame, Error> {
        let mut plain = vec![0u8; MAX_MSG];
        let mut data: Vec<u8> = Vec::new();
        let mut total: Option<usize> = None;
        loop {
            let n = read_msg(&mut self.io, &mut self.buf).await?;
            let m = self.state.read_message(self.nonce, &self.buf[..n], &mut plain).map_err(crypto)?;
            self.nonce += 1;
            let mut chunk = &plain[..m];
            if total.is_none() {
                if chunk.len() < 4 {
                    return Err(Error::Protocol("short frame start".into()));
                }
                let t = u32::from_le_bytes(chunk[..4].try_into().unwrap()) as usize;
                if t > MAX_FRAME {
                    return Err(Error::Protocol(format!("frame of {t} bytes is too large")));
                }
                data.reserve_exact(t);
                total = Some(t);
                chunk = &chunk[4..];
            }
            data.extend_from_slice(chunk);
            let t = total.unwrap();
            if data.len() == t {
                return Frame::from_bytes(data);
            }
            if data.len() > t {
                return Err(Error::Protocol("frame longer than announced".into()));
            }
        }
    }
}

impl<W: AsyncWrite + Unpin> SecureWriter<W> {
    pub async fn send(&mut self, f: &Frame) -> Result<(), Error> {
        let mut plain = Vec::with_capacity(4 + f.len());
        plain.extend_from_slice(&(f.len() as u32).to_le_bytes());
        plain.extend_from_slice(&(f.json.len() as u32).to_le_bytes());
        plain.extend_from_slice(&f.json);
        plain.extend_from_slice(&f.payload);
        for chunk in plain.chunks(MAX_PLAIN) {
            self.buf.resize(chunk.len() + TAG, 0);
            let n = self.state.write_message(self.nonce, chunk, &mut self.buf).map_err(crypto)?;
            self.nonce += 1;
            self.io.write_all(&(n as u16).to_le_bytes()).await?;
            self.io.write_all(&self.buf[..n]).await?;
        }
        self.io.flush().await?;
        Ok(())
    }
}

type Halves<S> = (SecureReader<ReadHalf<S>>, SecureWriter<WriteHalf<S>>);

fn halves<S: AsyncRead + AsyncWrite>(io: S, state: StatelessTransportState) -> Halves<S> {
    let state = Arc::new(state);
    let (r, w) = tokio::io::split(io);
    (
        SecureReader { io: r, state: state.clone(), nonce: 0, buf: Vec::new() },
        SecureWriter { io: w, state, nonce: 0, buf: Vec::new() },
    )
}

/// Dialer side: authenticates the listener by its known public key.
pub async fn connect<S: AsyncRead + AsyncWrite + Unpin>(
    mut io: S,
    me: &Keys,
    their_public: &[u8; 32],
) -> Result<Halves<S>, Error> {
    let mut hs = builder()
        .local_private_key(&me.private)
        .remote_public_key(their_public)
        .build_initiator()
        .map_err(crypto)?;
    let mut buf = vec![0u8; MAX_MSG];
    let n = hs.write_message(&[], &mut buf).map_err(crypto)?;
    write_msg(&mut io, &buf[..n]).await?;
    let mut msg = Vec::new();
    let n = read_msg(&mut io, &mut msg).await?;
    hs.read_message(&msg[..n], &mut buf).map_err(crypto)?;
    let state = hs.into_stateless_transport_mode().map_err(crypto)?;
    Ok(halves(io, state))
}

/// Listener side: returns the dialer's public key once `accept_key` approved it.
pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(
    mut io: S,
    me: &Keys,
    accept_key: impl Fn(&[u8; 32]) -> bool,
) -> Result<([u8; 32], SecureReader<ReadHalf<S>>, SecureWriter<WriteHalf<S>>), Error> {
    let mut hs = builder().local_private_key(&me.private).build_responder().map_err(crypto)?;
    let mut msg = Vec::new();
    let mut buf = vec![0u8; MAX_MSG];
    let n = read_msg(&mut io, &mut msg).await?;
    hs.read_message(&msg[..n], &mut buf).map_err(crypto)?;
    let theirs = to32(hs.get_remote_static().ok_or_else(|| Error::Crypto("no static key".into()))?);
    if !accept_key(&theirs) {
        return Err(Error::NotLinked);
    }
    let n = hs.write_message(&[], &mut buf).map_err(crypto)?;
    write_msg(&mut io, &buf[..n]).await?;
    let state = hs.into_stateless_transport_mode().map_err(crypto)?;
    let (r, w) = halves(io, state);
    Ok((theirs, r, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_key_is_derived_from_private() {
        let k = Keys::generate();
        assert_eq!(Keys::from_private(k.private).public, k.public);
    }

    #[tokio::test]
    async fn linked_peers_exchange_large_frames_both_ways() {
        let (a, b) = (Keys::generate(), Keys::generate());
        let (ia, ib) = tokio::io::duplex(1 << 16);
        let a_pub = a.public;
        let b2 = b.clone();
        let server = tokio::spawn(async move { accept(ib, &b2, |k| *k == a_pub).await.unwrap() });
        let (mut ra, mut wa) = connect(ia, &a, &b.public).await.unwrap();
        let (who, mut rb, mut wb) = server.await.unwrap();
        assert_eq!(who, a.public);
        let big: Vec<u8> = (0..3_000_000u32).map(|i| i as u8).collect();
        let f = Frame::new(&serde_json::json!({"t": "x"}), big.clone());
        let send = tokio::spawn(async move {
            wa.send(&f).await.unwrap();
            wa.send(&Frame::new(&2u8, vec![])).await.unwrap();
            wa
        });
        let got = rb.recv().await.unwrap();
        assert_eq!(got.payload, big);
        assert_eq!(rb.recv().await.unwrap().msg::<u8>().unwrap(), 2);
        send.await.unwrap();
        wb.send(&Frame::new(&1u8, vec![])).await.unwrap();
        assert_eq!(ra.recv().await.unwrap().msg::<u8>().unwrap(), 1);
    }

    #[tokio::test]
    async fn unknown_dialer_is_refused() {
        let (a, b) = (Keys::generate(), Keys::generate());
        let (ia, ib) = tokio::io::duplex(1 << 16);
        let b2 = b.clone();
        let server = tokio::spawn(async move { accept(ib, &b2, |_| false).await.map(|_| ()) });
        let client = connect(ia, &a, &b.public).await;
        assert!(matches!(server.await.unwrap(), Err(Error::NotLinked)));
        assert!(client.is_err(), "dialer must not get a session");
    }

    #[tokio::test]
    async fn wrong_listener_key_fails_handshake() {
        let (a, b, c) = (Keys::generate(), Keys::generate(), Keys::generate());
        let (ia, ib) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move { accept(ib, &b, |_| true).await.map(|_| ()) });
        assert!(connect(ia, &a, &c.public).await.is_err());
        assert!(server.await.unwrap().is_err());
    }
}

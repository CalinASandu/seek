//! Hub addresses and the dialer. A PC reaches the hub either directly (`host:port`, the LAN or a tunnel
//! that is already encrypted) or through Tailscale Funnel (`tls://host`, TLS-terminated on 443). The Noise
//! session always runs inside whatever stream comes out of [`dial`].

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::Error;

/// The hub's default listen port when an address names none.
const DEFAULT_PORT: u16 = 47321;
const TLS_PORT: u16 = 443;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubAddr {
    /// `host:port`, no TLS.
    Plain(String),
    Tls { host: String, port: u16 },
}

impl HubAddr {
    /// Accepts `tls://host[:port]`, `https://host[/path]` (both TLS, default 443), a bare `*.ts.net` host
    /// (TLS) or `host[:port]` (plain, default 47321). The scheme is case-insensitive and hosts are
    /// lowercased.
    pub fn parse(s: &str) -> Result<HubAddr, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty hub address".into());
        }
        let (scheme, rest) = match s.split_once("://") {
            Some((scheme, rest)) => (Some(scheme.to_ascii_lowercase()), rest),
            None => (None, s),
        };
        if let Some(scheme) = &scheme {
            if scheme != "tls" && scheme != "https" {
                return Err(format!("unsupported scheme `{scheme}://` (use tls://)"));
            }
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port: u16 = port.parse().map_err(|_| format!("bad port `{port}`"))?;
                if port == 0 {
                    return Err("port 0".into());
                }
                (host, Some(port))
            }
            None => (authority, None),
        };
        let host = host.to_ascii_lowercase();
        let valid = !host.is_empty()
            && !host.starts_with(['.', '-'])
            && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !valid {
            return Err(format!("bad host `{host}`"));
        }
        if scheme.is_some() || host.ends_with(".ts.net") {
            Ok(HubAddr::Tls { host, port: port.unwrap_or(TLS_PORT) })
        } else {
            Ok(HubAddr::Plain(format!("{host}:{}", port.unwrap_or(DEFAULT_PORT))))
        }
    }
}

impl fmt::Display for HubAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubAddr::Plain(addr) => f.write_str(addr),
            HubAddr::Tls { host, port } if *port == TLS_PORT => write!(f, "tls://{host}"),
            HubAddr::Tls { host, port } => write!(f, "tls://{host}:{port}"),
        }
    }
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// Client-side TLS settings: TLS 1.3 only, certificate chain and hostname always checked.
#[derive(Clone)]
pub struct TlsConfig(Arc<rustls::ClientConfig>);

impl fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TlsConfig")
    }
}

impl TlsConfig {
    fn with_store(roots: rustls::RootCertStore) -> TlsConfig {
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("ring supports TLS 1.3")
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsConfig(Arc::new(cfg))
    }

    /// The public web PKI (what Tailscale Funnel's Let's Encrypt certificates chain to).
    pub fn public() -> TlsConfig {
        TlsConfig::with_store(rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()))
    }

    /// Trusts exactly one DER root certificate (tests).
    pub fn with_root(der: &[u8]) -> TlsConfig {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(rustls::pki_types::CertificateDer::from(der.to_vec())).expect("valid root certificate");
        TlsConfig::with_store(roots)
    }
}

/// Opens a stream to the hub: TCP, plus TLS for `tls://` addresses (never a plain fallback). The whole
/// connect + handshake is bounded by `timeout`. Certificate and hostname failures are [`Error::Untrusted`].
pub async fn dial(addr: &HubAddr, tls: &TlsConfig, timeout: Duration) -> Result<Box<dyn Stream>, Error> {
    match tokio::time::timeout(timeout, dial_inner(addr, tls, timeout)).await {
        Ok(r) => r,
        Err(_) => Err(Error::Io(std::io::ErrorKind::TimedOut.into())),
    }
}

async fn dial_inner(addr: &HubAddr, tls: &TlsConfig, timeout: Duration) -> Result<Box<dyn Stream>, Error> {
    match addr {
        HubAddr::Plain(hp) => {
            let tcp = TcpStream::connect(hp.as_str()).await?;
            let _ = tcp.set_nodelay(true);
            Ok(Box::new(tcp))
        }
        HubAddr::Tls { host, port } => {
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| Error::Protocol(format!("bad hub host `{host}`: {e}")))?;
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), *port)).await?.collect();
            dial_tls(&addrs, name, tls, timeout).await
        }
    }
}

/// Tries each address in turn, sharing `timeout` between them. A Funnel name resolves to several relays, and one
/// that accepts TCP and then drops the handshake must not hide the others. A certificate failure ends the attempt.
async fn dial_tls(
    addrs: &[SocketAddr],
    name: rustls::pki_types::ServerName<'static>,
    tls: &TlsConfig,
    timeout: Duration,
) -> Result<Box<dyn Stream>, Error> {
    let each = timeout / addrs.len().max(1) as u32;
    let mut last = Error::Io(std::io::ErrorKind::NotFound.into());
    for addr in addrs {
        let attempt = async {
            let tcp = TcpStream::connect(addr).await?;
            let _ = tcp.set_nodelay(true);
            tokio_rustls::TlsConnector::from(tls.0.clone()).connect(name.clone(), tcp).await.map_err(tls_error)
        };
        match tokio::time::timeout(each, attempt).await {
            Ok(Ok(stream)) => return Ok(Box::new(stream)),
            Ok(Err(e @ Error::Untrusted(_))) => return Err(e),
            Ok(Err(e)) => last = e,
            Err(_) => last = Error::Io(std::io::ErrorKind::TimedOut.into()),
        }
    }
    Err(last)
}

/// rustls reports handshake failures as an `io::Error` wrapping a `rustls::Error`.
fn tls_error(e: std::io::Error) -> Error {
    match e.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
        Some(rustls::Error::InvalidCertificate(why)) => Error::Untrusted(format!("{why:?}")),
        _ => Error::Io(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{noise, Frame, Keys};
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::net::TcpListener;

    #[test]
    fn hub_addr_parsing() {
        let tls = |h: &str, p: u16| HubAddr::Tls { host: h.into(), port: p };
        assert_eq!(HubAddr::parse("https://seek-hub.x.ts.net/").unwrap(), tls("seek-hub.x.ts.net", 443));
        assert_eq!(HubAddr::parse("seek-hub.x.ts.net").unwrap(), tls("seek-hub.x.ts.net", 443));
        // scheme and host are case-insensitive; the canonical form is lowercase, and 443 is implied
        let a = HubAddr::parse("TLS://Host:443").unwrap();
        assert_eq!(a, tls("host", 443));
        assert_eq!(a.to_string(), "tls://host");
        assert_eq!(HubAddr::parse("192.168.1.5").unwrap(), HubAddr::Plain("192.168.1.5:47321".into()));
        assert_eq!(HubAddr::parse("192.168.1.5:47321").unwrap(), HubAddr::Plain("192.168.1.5:47321".into()));
        assert_eq!(HubAddr::parse("tls://h:8443").unwrap(), tls("h", 8443));
        assert_eq!(HubAddr::parse("  https://h/some/path?x=1 ").unwrap(), tls("h", 443));
        assert_eq!(HubAddr::parse("h:8443").unwrap().to_string(), "h:8443");
        assert_eq!(tls("h", 8443).to_string(), "tls://h:8443");
        for bad in ["", "   ", "tls://", "http://h", "h:notaport", "h:0", "a b", ":47321"] {
            assert!(HubAddr::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    struct Pki {
        root_der: Vec<u8>,
        chain: Vec<CertificateDer<'static>>,
        key: Vec<u8>,
    }

    /// A throwaway CA plus a server certificate for `name` signed by it.
    fn pki(name: &str) -> Pki {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(vec![]).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec![name.to_string()]).unwrap().signed_by(&leaf_key, &ca, &ca_key).unwrap();
        Pki { root_der: ca.der().to_vec(), chain: vec![leaf.der().clone()], key: leaf_key.serialize_der() }
    }

    fn acceptor(p: &Pki, versions: &[&'static rustls::SupportedProtocolVersion]) -> tokio_rustls::TlsAcceptor {
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(versions)
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(p.chain.clone(), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(p.key.clone())))
            .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(cfg))
    }

    /// TLS server on an ephemeral loopback port that runs the Noise listener inside and echoes one frame.
    async fn serve(p: &Pki, versions: &[&'static rustls::SupportedProtocolVersion], keys: Keys) -> u16 {
        let acc = acceptor(p, versions);
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((tcp, _)) = l.accept().await else { return };
            let Ok(tls) = acc.accept(tcp).await else { return };
            let Ok((_, mut r, mut w)) = noise::accept(tls, &keys, |_| true).await else { return };
            if let Ok(f) = r.recv().await {
                let _ = w.send(&f).await;
            }
        });
        port
    }

    fn localhost(port: u16) -> HubAddr {
        HubAddr::Tls { host: "localhost".into(), port }
    }

    const T: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn tls_dial_then_noise() {
        let p = pki("localhost");
        let (hub, pc) = (Keys::generate(), Keys::generate());
        let port = serve(&p, &[&rustls::version::TLS13], hub.clone()).await;
        let stream = dial(&localhost(port), &TlsConfig::with_root(&p.root_der), T).await.unwrap();
        let (mut r, mut w) = noise::connect(stream, &pc, &hub.public).await.unwrap();
        let f = Frame::new(&serde_json::json!({"t": "hello"}), b"payload".to_vec());
        w.send(&f).await.unwrap();
        assert_eq!(r.recv().await.unwrap(), f);
    }

    #[tokio::test]
    async fn tls_dial_skips_a_relay_that_drops_or_stalls_the_handshake() {
        let p = pki("localhost");
        let dropper = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let staller = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addrs = [dropper.local_addr().unwrap(), staller.local_addr().unwrap()];
        tokio::spawn(async move {
            drop(dropper.accept().await);
            let held = staller.accept().await;
            tokio::time::sleep(T).await;
            drop(held);
        });
        let (hub, pc) = (Keys::generate(), Keys::generate());
        let port = serve(&p, &[&rustls::version::TLS13], hub.clone()).await;
        let good: SocketAddr = ([127, 0, 0, 1], port).into();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let tls = TlsConfig::with_root(&p.root_der);
        let stream = dial_tls(&[addrs[0], addrs[1], good], name, &tls, Duration::from_secs(3)).await.unwrap();
        noise::connect(stream, &pc, &hub.public).await.unwrap();
    }

    #[tokio::test]
    async fn tls_wrong_host_refused() {
        let p = pki("other.test");
        let port = serve(&p, &[&rustls::version::TLS13], Keys::generate()).await;
        let r = dial(&localhost(port), &TlsConfig::with_root(&p.root_der), T).await;
        assert!(matches!(r, Err(Error::Untrusted(_))), "{:?}", r.err());
    }

    #[tokio::test]
    async fn tls_untrusted_root_refused() {
        let p = pki("localhost");
        let port = serve(&p, &[&rustls::version::TLS13], Keys::generate()).await;
        let r = dial(&localhost(port), &TlsConfig::public(), T).await;
        assert!(matches!(r, Err(Error::Untrusted(_))), "{:?}", r.err());
    }

    #[tokio::test]
    async fn tls_12_refused() {
        let p = pki("localhost");
        let port = serve(&p, &[&rustls::version::TLS12], Keys::generate()).await;
        assert!(dial(&localhost(port), &TlsConfig::with_root(&p.root_der), T).await.is_err());
    }

    #[tokio::test]
    async fn plain_dial_reaches_a_listener() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = HubAddr::Plain(l.local_addr().unwrap().to_string());
        let accepted = tokio::spawn(async move { l.accept().await.is_ok() });
        dial(&addr, &TlsConfig::public(), T).await.unwrap();
        assert!(accepted.await.unwrap());
    }
}

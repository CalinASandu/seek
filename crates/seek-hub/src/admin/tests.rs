use seek_sync::Keys;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::store::PcRecord;

struct Env {
    _dir: tempfile::TempDir,
    cfg: HubConfig,
    hub: Arc<Hub>,
    store: Arc<Store>,
    admin: Arc<Admin>,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let cfg = HubConfig {
        name: "Test Hub".into(),
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        public: None,
        data_dir: dir.path().join("data"),
        keys_dir: dir.path().join("keys"),
        pc_quota_bytes: 2048 * 1024 * 1024,
        pc_max_devices: 64,
    };
    let key = HubKey::create(&cfg.keys_dir).unwrap();
    let store = Arc::new(Store::open(&cfg, key).unwrap());
    let hub = Hub::start(cfg.clone(), store.clone()).await.unwrap();
    let admin = Admin::new(hub.clone(), store.clone(), cfg.clone());
    Env { _dir: dir, cfg, hub, store, admin }
}

fn link_pc(store: &Store, n: u128, name: &str) -> Uuid {
    let rec = PcRecord {
        host_id: Uuid::from_u128(n),
        name: name.into(),
        public_key: seek_sync::peers::key_to_b64(&Keys::generate().public),
        user_sid: "S-1-5".into(),
        user_name: "me".into(),
        profiles: vec![],
        last_seen_ms: 7,
    };
    store.add_pc(rec).unwrap();
    Uuid::from_u128(n)
}

/// Sends `line` through `serve_conn` (expecting `token`) and returns the raw reply.
async fn raw(e: &Env, token: Option<&str>, line: &[u8]) -> String {
    let (mut client, server) = duplex(64 * 1024);
    let admin = e.admin.clone();
    let token = token.map(str::to_string);
    let task = tokio::spawn(async move { serve_conn(server, token.as_deref(), &admin).await });
    client.write_all(line).await.unwrap();
    let mut out = String::new();
    client.read_to_string(&mut out).await.unwrap();
    task.await.unwrap();
    out
}

fn reply(text: &str) -> AdminResp {
    serde_json::from_str(text.trim_end()).unwrap_or_else(|e| panic!("{e}: {text}"))
}

#[tokio::test]
async fn wrong_token_refused_and_nothing_done() {
    let e = env().await;
    let r = raw(&e, Some("secret"), b"{\"token\":\"secreT\",\"req\":{\"cmd\":\"link\"}}\n").await;
    assert!(matches!(reply(&r), AdminResp::Error { .. }), "{r}");
    assert!(!e.hub.pairing_open(), "a request with a wrong token opened pairing");

    let r = raw(&e, Some("secret"), b"{\"token\":\"secret\",\"req\":{\"cmd\":\"link\"}}\n").await;
    assert!(matches!(reply(&r), AdminResp::Link(_)), "{r}");
    assert!(e.hub.pairing_open());
}

#[tokio::test]
async fn oversized_or_bad_request_refused() {
    let e = env().await;
    let mut long = vec![b'a'; MAX_REQUEST + 10];
    long.push(b'\n');
    match reply(&raw(&e, None, &long).await) {
        AdminResp::Error { message } => assert!(message.contains("longer"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(reply(&raw(&e, None, b"{\"req\":{\"cmd\":\"fly\"}}\n").await), AdminResp::Error { .. }));
    // no newline, then EOF: the partial line is still parsed, and gets an error reply
    let (mut client, server) = duplex(1024);
    let admin = e.admin.clone();
    let task = tokio::spawn(async move { serve_conn(server, None, &admin).await });
    client.write_all(b"{\"req\":").await.unwrap();
    client.shutdown().await.unwrap();
    let mut out = String::new();
    client.read_to_string(&mut out).await.unwrap();
    task.await.unwrap();
    assert!(matches!(reply(&out), AdminResp::Error { .. }), "a partial request gets an error reply: {out}");
}

#[test]
fn constant_time_compare() {
    assert!(same(b"abc", b"abc"));
    assert!(!same(b"abc", b"abd"));
    assert!(!same(b"abc", b"ab"));
    assert!(same(b"", b""));
}

#[tokio::test]
async fn unlink_by_name_or_id() {
    let e = env().await;
    let desk = link_pc(&e.store, 1, "Desk");
    let a = link_pc(&e.store, 2, "Laptop");
    link_pc(&e.store, 3, "laptop");

    let r = e.admin.handle(AdminReq::Unlink { pc: "desk".into() }).await;
    assert_eq!(r, AdminResp::Unlinked { name: "Desk".into(), host_id: desk });
    assert!(matches!(e.admin.handle(AdminReq::Unlink { pc: "Desk".into() }).await, AdminResp::Error { .. }));
    match e.admin.handle(AdminReq::Unlink { pc: "LAPTOP".into() }).await {
        AdminResp::Error { message } => assert!(message.contains(&a.to_string()), "{message}"),
        other => panic!("{other:?}"),
    }
    let r = e.admin.handle(AdminReq::Unlink { pc: a.to_string() }).await;
    assert_eq!(r, AdminResp::Unlinked { name: "Laptop".into(), host_id: a });
    assert_eq!(e.store.pcs().len(), 1);
}

#[tokio::test]
async fn status_counts_pcs() {
    let e = env().await;
    link_pc(&e.store, 1, "Desk");
    let AdminResp::Status(s) = e.admin.handle(AdminReq::Status).await else { panic!() };
    assert_eq!((s.pc_count, s.pcs.len(), s.pcs[0].name.as_str(), s.pcs[0].last_seen_ms), (1, 1, "Desk", 7));
    assert_eq!(s.name, "Test Hub");
    assert_eq!(s.listening, vec![e.hub.local_addrs()[0].to_string()]);
    assert!(!s.lan && !s.pairing.open);
    let AdminResp::Pcs { pcs } = e.admin.handle(AdminReq::Pcs).await else { panic!() };
    assert_eq!(pcs, s.pcs);
}

#[tokio::test]
async fn request_over_the_endpoint() {
    let e = env().await;
    assert!(matches!(request(&e.cfg.data_dir, AdminReq::Status).await, Err(ClientError::NotRunning(_))));
    let lock = crate::lock::DataLock::try_take(&e.cfg.data_dir).unwrap();
    let l = AdminListener::bind(&e.cfg.data_dir, &lock).await.unwrap();
    let task = tokio::spawn(l.run(e.admin.clone()));
    let r = request(&e.cfg.data_dir, AdminReq::Status).await.unwrap();
    assert!(matches!(r, AdminResp::Status(s) if s.pc_count == 0));
    task.abort();
    remove_endpoint(&e.cfg.data_dir);
    assert!(matches!(request(&e.cfg.data_dir, AdminReq::Status).await, Err(ClientError::NotRunning(_))));
}

#[tokio::test]
async fn rotate_key_live() {
    let e = env().await;
    link_pc(&e.store, 1, "Desk");
    let old = HubKey::load(&e.cfg.keys_dir).unwrap();
    let r = e.admin.handle(AdminReq::RotateKey).await;
    assert_eq!(r, AdminResp::Rotated { files: 3, resumed: false });
    let new = HubKey::load(&e.cfg.keys_dir).unwrap();
    assert_ne!(new.id(), old.id());
    assert!(!e.cfg.keys_dir.join(crate::crypt::NEW_KEY_FILE).exists());
    // the running store keeps writing with the new key
    link_pc(&e.store, 2, "Laptop");
    let (n, bad) = rotate::verify_dir(&e.cfg.data_dir, &[&new]).unwrap();
    assert_eq!((n, bad.len()), (3, 0), "{bad:?}");
}

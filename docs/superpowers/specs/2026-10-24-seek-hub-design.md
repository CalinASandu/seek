# Seek Hub — design

Date: 2026-10-24 · Status: draft for review · Builds on `2026-10-11-seek-sync-design.md`

## Summary

- **Seek Hub** is a small headless server, run in **Docker** (first target: Raspberry Pi 4, 8 GB, 64-bit; any
  Linux amd64/arm64 box works). It is the single point of truth between your PCs: every PC syncs **only with the
  hub**, never with other PCs, once a hub is set.
- **Truth rule**: each PC is the source of truth for its own drives; the hub is the source of truth for every
  other PC's drives. The hub accepts changes to a drive only from the PC that owns it.
- **The hub never starts anything.** It never dials out, never pushes. PCs connect to it, send their own changes
  and ask for the others'; the hub only answers. Same Noise IK encryption and code pairing as LAN sync (8 digits for the hub).
- The hub stores encrypted copies of the indexes (key on the hub), file names only, never file contents.
- Reachable from anywhere through **Tailscale Funnel** (a public HTTPS-grade endpoint, `seek-hub.<tailnet>.ts.net:443`)
  run by a Tailscale sidecar container on the hub — no port forwarding, and **PCs need nothing installed** besides
  Seek. Inside Funnel's TLS, every connection is still Noise IK with keys from pairing, so the tunnel (or anyone who
  breaks TLS) learns nothing and cannot impersonate either side. Optional direct LAN address for speed at home.
- Setup: `docker compose up -d`, then (optionally `init`, hub stopped) `docker exec seek-hub seek-hub link`
  (prints a code you type into the Windows app's *Link a device → Hub*).
- Without a hub, LAN sync works exactly as today.
- Reserved for later, not built now: relaying (SFTP between PCs through the hub), indexing the hub's own disks,
  a web UI, macOS.

## 1. Decisions (from the user, 2026-10-22 … 24)

| # | Decision |
|---|---|
| 1 | Runs in Docker; Pi 4 (8 GB) first, configurable for any Linux host. |
| 2 | Hub = single point of truth. A PC is authoritative for its own drives, the hub for everyone else's. |
| 3 | The hub never initiates: no outbound connections, no unrequested messages. PCs dial and ask. |
| 4 | Hub stores index copies, encrypted at rest with a key that lives on the hub (protects a stolen SD card / disk; the running hub can read them). |
| 5 | With a hub, PCs do not sync with each other directly. LAN sync only when there is no hub. |
| 6 | The hub is exposed to the public internet through a tunnel (Tailscale Funnel), so PCs need no VPN client and no port forwarding. Made as secure as possible (§9). |
| 7 | No web UI in this spec. |
| 8 | Linking a PC to the hub removes its old direct LAN links. |
| 9 | One user (you) and your devices; no accounts. |
| 10 | Later: relay (for SFTP), indexing the hub's own disks. The protocol leaves room for relay now. |
| 11 | No macOS work (no Mac to test on). |

## 2. Goals and non-goals

**Goals**
1. A PC linked to the hub sees every other linked PC's drives (online or offline), with no PC-to-PC connection.
2. Works from anywhere through the public Funnel endpoint, PCs behind any NAT (they only dial out), with nothing
   but Seek installed on the PCs; a direct LAN address is used first when reachable.
3. Hub runs unattended: survives restarts and power cuts, starts with the container, needs no unlock step.
4. Safe to expose publicly (§9): assume the endpoint is known to attackers (Funnel names appear in public
   certificate logs). Only paired keys get past the handshake, nothing is parsed before it, ownership and quotas
   are enforced, the hub has no outbound traffic of its own, and its container is non-root and read-only.
5. Small footprint: idle RAM < 100 MB even with several million stored entries.
6. One-command setup and a few admin commands; multi-arch image (`linux/arm64`, `linux/amd64`).

**Non-goals (this spec)**
- Web UI, searching on the hub, opening/downloading files.
- Relaying connections between PCs (protocol room only, §6.5).
- Indexing the hub's own disks; Linux/macOS desktop app.
- Multiple users / accounts; mixing hub and direct LAN links on one PC.

## 3. Components

| Unit | Responsibility | Depends on |
|---|---|---|
| `crates/seek-sync` (changes) | protocol v2: `Hello` carries a role (`Pc` / `Hub`); new hub messages (§6); a PC-side **hub session** (push own drives, poll for others); an **ownership check** helper; **TLS dialer** for `tls://` hub addresses (Noise runs inside it) | `rustls`, `tokio-rustls`, `webpki-roots` (pure Rust, no Windows deps) |
| `crates/seek-hub` (new, Linux-first, no Windows deps; also builds on Windows so tests run in `cargo test --workspace`) | daemon: TCP listener, hub sessions, encrypted store (§5), pairing window, compaction; admin CLI subcommands (`init`, `link`, `status`, `pcs`, `unlink`, `rotate-key`, `doctor`) talking to the daemon over a Unix socket inside the container | `seek-core`, `seek-sync`, `tokio`, `chacha20poly1305` |
| `seek-service` (changes) | hub mode: dial the hub, run the PC side of the hub session, stop LAN listener / mDNS / direct sessions while a hub is set, remove direct links on hub link, store the hub in `peers.json` | `seek-sync` |
| `seek-ipc` | `LinkHub{addr, code}`, `UnlinkHub`, `HubStatus`; `PeersChanged` event also covers the hub | — |
| `app/` | *Link a device* gains a **Hub** tab (address + code); Settings → Linked PCs shows the hub and the PCs it knows; status bar `Hub · synced 20 s ago` | — |
| `docker/` | `Dockerfile` (static musl binary on `scratch`), `compose.yaml` (hub + Tailscale sidecar with Funnel), `funnel.json` (serve config), `tailnet-policy.example.hujson`, `.env.example` | — |
| `scripts/build-hub-image.ps1` | cross-compile `aarch64`/`x86_64-unknown-linux-musl` (cargo-zigbuild), build + push or `docker save` a multi-arch image | — |

## 4. Identity, pairing, trust

- The hub has a `host_id` (UUID), a name (default `Seek Hub`) and an X25519 static key, like a PC.
- **Linking** (`seek-hub link`): the daemon opens a pairing window and prints the code and the addresses it can be
  reached at (`tls://seek-hub.<tailnet>.ts.net`, optional LAN `ip:47321`). Pairing is closed at all other times —
  the hub answers no pairing request unless an admin is running `link`. The code is **8 digits**, valid 2 min, and
  **3 wrong attempts in total** (not per client) close the window, so a stranger on the public endpoint gets at
  most 3 guesses in 10⁸ per `link` run. In the app: *Link a device → Hub*, paste the address, type the code. SPAKE2 over the code, then both sides exchange `host_id`, name, static key and
  (from the PC) the linking user's SID + name. Answering a pairing request the PC started is not "initiating".
- The hub stores PCs in its own `peers` file; the PC stores the hub as a peer with `role: Hub`.
- **On successful hub link the PC deletes all its direct links** (it ends its direct LAN sessions **without**
  sending `Unlinked`, so those peers keep its copies of their drives) and keeps the copies it had of their drives until the hub delivers fresher ones
  (copies are matched by `DeviceId`, so nothing is fetched twice). A PC can have only one hub.
- Every PC links to the hub with its own code. The hub never trusts a key it did not pair with itself (no group
  join / vouching through the hub).
- **One user**: the SID of whoever linked each PC is recorded; all of them count as the same person for the access
  rule (§7).
- Reinstalled PC (new key) → its key is unknown, so it is refused like any stranger (the hub cannot see its
  `host_id` before the handshake). Linking it again with a new code replaces the old record; if the `host_id` is
  the same, its stored drives stay its own. Reinstalled hub (new key) → the PC's handshake fails and it shows
  *Hub needs re-linking*.
- The hub's pairing reply carries its addresses (LAN ones when enabled, then the public one), so the PC stores
  them all and tries LAN first.

## 5. Hub storage

All under the `/data` volume; every file encrypted (XChaCha20-Poly1305, random nonce per file write, file header
`SEEKENC1 | key id | nonce`), written atomically (temp + rename).

| File | Content |
|---|---|
| `hub.json` | hub id, name, settings |
| `host.key` | Noise static key (encrypted like the rest) |
| `pcs.json` | linked PCs: id, name, public key, linking SID + user name, profiles, last seen, state |
| `devices.json` | catalog: every stored drive record (`Device`, owner PC) |
| `copies/<device>.full` | last full copy as received (`.seekidx`, zstd) + its cursor |
| `copies/<device>.journal` | delta batches since that full copy, with the owner's `(generation, seq)` numbering |

- **The hub keeps copies as bytes, not as live indexes.** It never decodes or searches them in normal operation,
  so idle RAM stays small and the hub has no query engine to attack.
- **Serving a copy**: cursor in the journal's range → `Delta` from the journal; otherwise → the stored full copy
  then a `Delta` from its cursor to the newest (two messages, the PC applies both).
- **Compaction**: when a journal grows past 25% of its full copy's size (or 50 MB), the hub decodes that one device,
  applies the journal, re-encodes and replaces both files. One device at a time, in a blocking thread, low
  priority. 2M entries on a Pi 4 should take < 10 s and < 600 MB peak (measured in the plan).
- **New full copy from the owner** (rescan / new generation) replaces both files.
- **Key**: `/keys/hub.key` (32 random bytes), a separate mount so it can live somewhere other than the data volume
  (a USB stick, a Docker secret, or simply another folder — documented trade-off: on the same card it only stops
  casual reading, not someone holding the card). Created by `init` if missing. `rotate-key` re-encrypts everything.
  Data without its key is unreadable → `doctor` says so; the fix is to wipe `/data` and re-link (PCs re-upload).

## 6. Protocol v2 (hub sessions)

Noise `IK`, length-prefixed frames, postcard/zstd payloads as today. `SYNC_PROTOCOL` → 2; `Hello` gains `role`.
LAN PC↔PC sessions keep working on v2 with `role: Pc` (both PCs must be updated — one user).

**Hub addresses** (stored with the hub peer, tried in order, the first that completes a handshake wins):
1. LAN `ip:47321` if configured — plain TCP + Noise (fast, no tunnel bandwidth limit);
2. `tls://<name>.ts.net[:443]` — TCP to Funnel, **TLS 1.3** verified against public roots and the hostname
   (rustls + webpki-roots), then Noise inside. Funnel's TLS-terminated TCP forwarder ends TLS in the sidecar on the
   hub and passes plain TCP to `127.0.0.1:47321`. TLS hides the traffic from the network; **Noise is what
   authenticates**: the PC knows the hub's static key from pairing, so even a forged certificate or a compromised
   tunnel cannot read, change or impersonate anything.
The transport is "any TLS-terminating TCP tunnel + Noise", so another tunnel (Caddy layer4, ngrok TLS endpoints,
a VPS with port 443) works the same way with a different address.

**The PC always dials; the hub only ever sends replies.** Every hub→PC message answers a specific PC message.

### 6.1 PC → hub: own drives (PC is the truth)
1. After `Hello`: PC sends `Devices{own devices, profiles}`. Hub replies `Have{cursors}` for those devices.
2. PC sends `Delta` / `Full` per device as in LAN sync; hub stores it, replies `Ack`.
3. Every 60 s (and on *Sync now*, and right after an index change, debounced 2 s) the PC sends new batches and an
   updated `Devices` when the list changed. Hub replies `Ack`.
4. **Ownership check** (hub side, enforced on every message): a PC may create, update or remove only devices whose
   owner is its own `host_id` — the key that authenticated the session. Devices claimed by another PC are refused
   with an error and logged. A device id first seen from PC A belongs to A forever (until A removes it or is unlinked).

### 6.2 hub → PC: other PCs' drives (hub is the truth)
1. PC sends `Pull{cursors of others' devices it holds}` right after its own upload, then every 60 s and on
   *Sync now*.
2. Hub replies `Catalog{pcs: [(host_id, name, linking user, profiles, last_seen)], devices}` — every other PC's
   drives — then per device `Delta` / `Full` (or nothing when up to date), then `PullDone`.
3. The PC applies them exactly like copies from a LAN peer (`SyncHost::remote_devices` / `apply_delta` /
   `apply_full`, owner = the original PC, not the hub). Devices missing from the catalog are dropped.
4. The PC ignores anything in a reply about its **own** devices (its own copy always wins), and the hub never
   sends them.
5. Polling cost when idle: one `Pull` + `Catalog` + `PullDone` per minute, target < 2 KB.

### 6.3 Unlinking
- From a PC (*Unlink hub*): PC sends `Unlinked`, hub deletes that PC and all its drives' copies; the PC drops the
  hub peer and the copies it got through it (other PCs' drives), returning to LAN mode with no links.
- From the hub (`seek-hub unlink <pc>`): hub deletes the PC and its copies and keeps the PC's key on a small
  tombstone list. When that PC next connects, the hub completes the handshake, replies `Unlinked` and closes (a
  reply, still not initiation); the PC shows *The hub unlinked this PC* and returns to LAN mode. Any other unknown
  key is refused during the handshake.
- Other PCs see the removed PC's drives disappear at their next `Pull`.

### 6.4 Errors
| Situation | Behaviour |
|---|---|
| Hub unreachable | PC keeps all copies, shows *Hub offline · synced 3 h ago*, retries every 60 s with backoff to 5 min |
| Drop mid-upload / mid-download | partial data discarded on that side; resumes from cursors next time |
| Corrupt copy on the hub (decrypt or decode fails) | hub drops that device's copy and answers the owner's next `Devices` with `Have{none}` → owner sends full |
| Device claimed by another PC | refused, logged, the claiming PC shows a warning |
| Protocol mismatch | *Update Seek on this PC* / *Update the hub* |
| Disk full / PC quota reached on the hub | uploads refused with `HubFull` / `QuotaFull`; PCs show it; downloads keep working |
| Funnel bandwidth limit (not configurable, slow full copies) | transfers continue slowly; LAN address used when at home; status shows progress |
| TLS certificate invalid / wrong host | connection refused, PC shows *Hub address not trusted*; never falls back to no TLS for a `tls://` address |
| Handshake limits hit (§9) | new connections refused for a short while; linked PCs retry with backoff |

### 6.5 Room for relay (not built)
- Message tags `open`, `data`, `close` (each with a `channel: u16`; payload = relayed bytes) are reserved in v2.
  The hub answers them with `refused` for now; no framing change is needed later.
- Intended later design: PC A asks the hub to open a channel to PC B; B must itself be connected and must have
  allowed it; the hub forwards bytes it cannot read (A and B run their own Noise session inside the channel). The
  hub still never dials: it only joins two sessions that both PCs opened. SFTP will ride on such a channel.

## 7. Search, duplicates and access on the PCs

Unchanged from LAN sync: copies from the hub are ordinary remote devices labelled with their owner PC; `pc:` and
`device:` filters work; the same USB stick seen by two PCs shows once (connected local record wins, else newest).
**Access**: elevated → everything; otherwise other PCs' profile folders are hidden except those of the linking
user, who is the same person on every PC linked to the hub (decision 9).

## 8. Docker and setup

```yaml
# docker/compose.yaml (abridged)
services:
  tailscale:                        # owns the network; Funnel lives here
    image: tailscale/tailscale:<pinned version>
    hostname: seek-hub              # → seek-hub.<tailnet>.ts.net
    environment:
      TS_AUTHKEY: "${TS_AUTHKEY}"   # tagged, one-off auth key (tag:seek-hub)
      TS_STATE_DIR: /var/lib/tailscale
      TS_USERSPACE: "true"          # no /dev/net/tun, no NET_ADMIN
      TS_SERVE_CONFIG: /config/funnel.json
    volumes: ["./tailscale:/var/lib/tailscale", "./funnel.json:/config/funnel.json:ro"]
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    # ports: ["47321:47321"]        # optional direct LAN access (uncomment + set SEEK_HUB_LAN=1)
    restart: unless-stopped
  seek-hub:
    image: ghcr.io/calinasandu/seek-hub:<version>
    network_mode: service:tailscale # reachable only through the sidecar
    read_only: true
    user: "10001:10001"
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    mem_limit: 1g
    pids_limit: 64
    volumes: ["./data:/data", "./keys:/keys:ro"]
    environment: { SEEK_HUB_PUBLIC: "${SEEK_HUB_PUBLIC:-}", SEEK_HUB_LAN: "${SEEK_HUB_LAN:-}" }   # empty = hub.json from init (R31); LISTEN/DATA/KEYS/PATH=/ are in the image
    healthcheck: { test: ["CMD", "/seek-hub", "status", "--quiet"], interval: 60s, timeout: 10s, start_period: 30s, retries: 3 }
    restart: unless-stopped
```
`funnel.json` publishes exactly one thing: TLS-terminated TCP on **443 → 127.0.0.1:47321** (equivalent of
`tailscale funnel --tls-terminated-tcp=443 tcp://127.0.0.1:47321`). No web handlers, nothing else. The plan's
first task verifies that `TS_SERVE_CONFIG` can express a TLS-terminated TCP Funnel; if it cannot, the sidecar runs
the `tailscale funnel` command once at start instead.

**Tailscale account setup (done once, guided by `init` and the README)**: enable MagicDNS + HTTPS certificates;
paste `tailnet-policy.example.hujson` (and **replace** the default allow-all rule with its two grants, member to member and member to `tag:seek-hub` on TCP 443, so your own devices keep reaching each other while the tagged hub is excluded; never just delete it. Remove any broader `funnel` nodeAttr too), which defines `tag:seek-hub`, grants the `funnel` attribute **only** to
that tag, and allows tailnet traffic to it on port 443 only (the published port; a PC running Tailscale resolves the
hub's name to its tailnet address, not to Funnel) and none from it; create a tagged, single-use auth key. Tagged
nodes do not expire.

**Flow**
1. Folders and key: `mkdir -p data keys tailscale && sudo chown 10001:10001 data keys && sudo chown root:root tailscale`
   (the hub runs as 10001; the sidecar is root with every capability dropped, so it needs a folder root owns; `data/`
   and `keys/` live on a Unix filesystem). Then, with the hub stopped,
   `docker run --rm -v "$PWD/data:/data" -v "$PWD/keys:/keys" <image> init --keys` creates `/keys/hub.key` once
   (`/keys` is read-only during normal running; R32), then `docker compose up -d` starts the sidecar, Funnel and an
   empty hub. If the key is missing the hub refuses to start and says how to create it.
2. Optional: `docker compose stop seek-hub; docker compose run --rm --no-deps seek-hub init; docker compose start
   seek-hub` (R24, R30) — hub name, quotas, optional LAN address; prints the public address and a checklist.
   Defaults work without it. `.env` values for `SEEK_HUB_PUBLIC` / `SEEK_HUB_LAN` win over what `init` saved (R31).
3. `docker exec -it seek-hub seek-hub link` — opens pairing for 2 min, prints the 8-digit code + addresses. All
   admin commands except `init` and `rotate-key` use `docker exec seek-hub seek-hub <cmd>` (image has `PATH=/`).
4. `seek-hub status` (PCs, last seen, stored drives and sizes, disk use, refused handshakes), `pcs`,
   `unlink <pc>`, `rotate-key` (hub stopped, writable `/keys`, like `init --keys`), `doctor` (key readable, store decrypts, listener up, Funnel answering with a valid
   certificate, policy tag present, free disk).

Image: static musl binary in a `scratch` image (no shell, no package manager), < 15 MB, `linux/arm64` +
`linux/amd64`. Binaries are built from Windows with cargo-zigbuild (no Docker needed there); the image is built on the Pi (`docker build --build-arg TARGETARCH=arm64`) or published to GHCR when the user says so (R33). Logs to stdout.

## 9. Security (public endpoint)

Threat model: anyone on the internet knows the address (Funnel hostnames are visible in public certificate
transparency logs) and can open connections; Tailscale's relays carry the bytes; the Pi can be stolen; one PC can
be stolen or compromised. Security never relies on the address being secret.

| Layer | Measure |
|---|---|
| Transport | TLS 1.3 to Funnel (public cert, hostname checked, no plain fallback for `tls://`); inside it Noise IK (`25519_ChaChaPoly_BLAKE2s`) gives end-to-end encryption, mutual authentication and forward secrecy between PC and hub. Tailscale and anyone on the path see only sizes and timing. |
| Who gets in | Only static keys in `pcs.json` complete the handshake (plus tombstoned keys, which only get `Unlinked`). Unknown keys are dropped after the first Noise message with no reply. |
| Before authentication | Nothing is parsed except the fixed-size Noise handshake (first message ≤ 256 bytes; anything larger closes the connection). No JSON, no version string, no banner — the hub says nothing to a stranger. |
| Pairing | Only while an admin runs `link`; 8-digit code, 2 min, 3 failed attempts in total close it; SPAKE2, so the code is never sent and a captured exchange cannot be brute-forced offline. |
| Flood / DoS limits | Handshake must finish in 5 s; at most 16 unauthenticated connections at once and 60 handshake attempts per minute (global — behind Funnel all clients look like localhost); one session per PC (a new one replaces the old); idle sessions closed after 3 min without traffic; authenticated frames capped (64 KB JSON, payload ≤ the configured per-device limit); Docker memory and process limits. Funnel's own bandwidth caps bound the rest. |
| A compromised PC | Can write only its own devices (ownership check); per-PC storage quota (default 2 GB) and device count (default 64); can read the other PCs' file names (as any linked PC can today) — `seek-hub unlink <pc>` cuts it off immediately and tombstones its key. |
| Stolen Pi / SD card | Everything at rest is encrypted (§5); the key can live off the card. With the key on the same card it is only obfuscation — stated plainly in `init`. |
| Hub itself | Never dials out (the sidecar does the tunnel); listens only on `127.0.0.1` inside the shared network namespace unless LAN is enabled; non-root, read-only root filesystem, all capabilities dropped, `no-new-privileges`, keys mounted read-only, `scratch` image. Sidecar runs in userspace networking mode (no tun device, no `NET_ADMIN`). |
| Tailnet isolation | Policy file: only `tag:seek-hub` may use Funnel; your own devices may reach it on TCP 443 only, and it may reach nothing. |
| Supply chain | Pinned image versions; `cargo deny` / `cargo audit` in the build script; dependencies kept to the existing crypto stack plus `rustls`. |
| Visibility | Counters for refused handshakes, failed pairing attempts, ownership violations, quota hits in `status`; logged at most once per minute per kind so an attacker cannot flood the log. |

Optional hardening the user can add later without code changes: run the hub on a separate VLAN; keep `/keys` on a
USB stick that is removed when not needed (the hub then refuses to start until it is back).

## 10. Testing

- `seek-sync`: v2 messages round trip; ownership check (own device ok, other PC's device refused, first claim
  wins); PC hub-session state machine against an in-memory fake hub; PC ignores data about its own devices.
- `seek-hub`: encrypted file round trip, wrong key → error, tamper → error; store serves delta / full + delta /
  nothing correctly; compaction result equals applying the journal; unlink + tombstone; pairing window
  (right / wrong code, expiry, attempts); hub never writes to a connection except in reply (test harness asserts
  every outgoing frame follows an incoming one).
- Integration (in one process, loopback): hub + two fake PCs → A uploads, B pulls and finds A's files; A changes
  → B gets a delta; B restarts → resumes with a delta; A rescans → B gets full; hub restarts → nothing re-uploaded;
  A claims B's device → refused; unlink A on the hub → B drops A's drives; linking to the hub removes direct links.
- Security tests (`seek-hub`): unknown key → closed with zero bytes written; oversized first message → closed;
  slow handshake → closed at 5 s; 17th unauthenticated connection refused; handshake rate limit; pairing refused
  when no window is open; 3 wrong codes from different connections close the window; quota and device-count limits;
  tombstoned key gets only `Unlinked`.
- TLS path: PC dialer against a local TLS-terminating proxy with a test CA → Noise inside works; wrong hostname or
  untrusted cert → refused, no fallback.
- `seek perf` (`== hub ==`): upload 2M full copy, pull 2M full copy, 1,000-change delta round trip, idle poll
  bytes; on the Pi additionally compaction time and peak RAM; one full copy through the real Funnel to see its
  bandwidth limit.
- Manual: hub on the user's Pi 4 in Docker with Funnel, two PCs, one on another network (e.g. phone hotspot) with
  no Tailscale installed.

## 11. Later

Relay channels + SFTP (§6.5); indexing the hub's own disks (needs the Linux walker/inotify path in `seek-scan`
and a Linux build of the service parts); web UI for searching from a browser/phone; other tunnels (Cloudflare
Tunnel would need a WebSocket transport, since it doesn't expose public raw TCP without a client app);
hub-introduced PCs (link once, migrate the whole LAN group); Linux desktop app; macOS.

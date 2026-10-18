# Seek sync between PCs — design

Date: 2026-10-11 · Status: draft for review · Builds on `2026-10-04-seek-local-design.md`

## Summary

- Searching on any of your PCs also finds files on your other PCs (and the drives they have seen), even when those
  PCs are off. Files are never transferred, only file lists.
- Link PCs with a **6-digit code**; no accounts. Linking one PC into a group links it with the whole group (max 5 PCs).
- **LAN only** for now (auto-discovery); a self-hosted server comes later and reuses the same sync logic.
- Each PC owns its drives and sends **only changes** (numbered per drive); a full copy is sent only the first time
  or after a full rescan. Every 60 s plus a **Sync now** button. Target: 2M-file full copy < 2 s, 1,000 changes < 50 ms.
- All traffic encrypted and mutually authenticated with keys made during linking.
- Access: *Run as administrator* sees everything on every PC; otherwise other users' profile folders are hidden
  (the person who linked counts as the same user on both PCs).
- New OS-independent crate `seek-sync` so Linux/macOS can follow.

## 1. Goals and non-goals

**Goals**
1. Search results include every linked PC's devices, labelled with PC and drive, online or offline.
2. Pairing with a one-time 6-digit code; up to 5 PCs per group.
3. LAN transport with automatic discovery (mDNS) and a manual IP fallback.
4. Every drive shared by default; per-drive opt-out; a global "Share this PC's files" switch.
5. Periodic sync (60 s) and on-demand **Sync now**; incremental, as fast as possible.
6. Encrypted, mutually authenticated connections; unlinked peers are refused.
7. Windows only now; the sync crate has no Windows dependencies.

**Non-goals (this spec)**
- Opening or downloading files from another PC.
- Internet / self-hosted server transport, relaying one PC's drives through another.
- macOS/Linux builds.
- Accounts, more than 5 PCs.

## 2. Components

| Unit | Responsibility | Depends on |
|---|---|---|
| `crates/seek-sync` (new, OS-independent) | pairing (SPAKE2 over the code), Noise session, wire messages, per-device change journal, sync state machine (decide delta vs full copy, apply), peer store (`peers.json`) | `seek-core`, `snow`, `spake2`, `zstd`, `postcard`, `tokio` |
| `seek-service::peers` (new) | TCP listener (port **47321**), mDNS advertise/browse (`mdns-sd`, `_seek._tcp`), one task per peer connection, 60-s tick, wiring journal ↔ indexes ↔ registry | `seek-sync`, service state |
| `seek-service` changes | record every applied change batch in the journal; remote devices in the registry; access rule for remote devices; new IPC requests | — |
| `seek-ipc` | requests: `StartPairing`, `JoinPairing{peer, code}`, `ListPeers`, `DiscoveredPeers`, `Unlink`, `SyncNow`, `SetSharing{enabled}`, `SetDeviceShared{device, shared}`; event `PeersChanged` | — |
| `app/` | sidebar grouped by PC, Sync now, Settings → Linked PCs, device-menu opt-out, remote row menu | — |
| Installer / `seek-service install` | firewall rule: inbound TCP 47321 for `seek-service.exe`, **Private** profile only; removed on uninstall | — |

## 3. Data

- **Host identity**: existing `host_id` (UUID in the registry) + a new X25519 static key pair in
  `%ProgramData%\Seek\host.key` (readable by SYSTEM/Administrators only).
- **`peers.json`**: per peer: `host_id`, `name`, `public_key`, `last_addr`, `linked_users: [(local_sid, remote_sid)]`,
  `group_id`, `last_sync_ms`, `state` (`Linked` / `NeedsRelink`).
- **Remote devices**: ordinary `Device` records whose existing `indexed_by_host` field is the owner PC (local
  devices carry this PC's `host_id`), keeping their original `DeviceId` (random UUIDs, globally unique). Indexes stored in `indexes\<id>.seekidx` as today, with
  the sync cursor in the index header.
- **Remote profiles**: each owner sends its ProfileList `(sid, path)` per device mount; stored with the peer.
- **Change journal** (owned devices only): per device a `generation: Uuid` (new on every full scan / replace / reformat)
  and a `seq: u64` incremented per applied batch. Batches kept in memory and appended to `journal\<device>.log`;
  capped at ~50 MB per device (oldest batches dropped, `first_seq` advanced). Unreadable journal → new generation.
- **Cursor** (per copied device): `(generation, seq)` last applied.

## 4. Pairing and security

1. PC A, *Link a PC* → service creates a code (6 digits, valid 2 min, 3 attempts) and listens for a pairing request.
2. PC B lists discovered PCs (or takes an IP), user picks A and enters the code.
3. SPAKE2 on the code → shared key → over that, both send `host_id`, name, static public key, the SID + user name of
   the linking user (from the pipe caller, `access::identify`), and the current group member list. Key confirmation
   on both sides, then both write `peers.json`.
4. **Group join**: A sends B the other members (keys, names, addresses); A tells each existing member about B over
   their authenticated sessions. Group is capped at 5; joining a full group fails with a clear message.
5. **Sessions**: Noise `IK` (`snow`, `Noise_IK_25519_ChaChaPoly_BLAKE2s`): the dialer knows the listener's static
   key; the listener learns the dialer's key from the (encrypted) first message and accepts only keys in
   `peers.json`, so both sides are authenticated. Unknown key → refused; a known `host_id` with a different key →
   peer marked `NeedsRelink`.
6. Handshake payload carries a protocol version; incompatible → "Update Seek on <PC>".
7. Linking requires the user's window (non-elevated is fine); the service performs all crypto.

## 5. Sync protocol

Framing: length-prefixed Noise messages (≤ 64 KB each; larger payloads chunked). Payloads: `postcard`, large ones
`zstd` level 1 streamed. One connection per peer pair (the PC with the lower `host_id` dials; the other accepts),
multiplexed by device.

1. **Hello**: each side sends its shared device list (records + profiles) and, for every device of the other side it
   holds, its cursor `(generation, seq)`.
2. **Per device**, the owner answers:
   - same generation and `seq ≥ first_seq − 1` → `Delta{from, to, batches}`;
   - otherwise → `Full{generation, seq, index}` (`encode_index` bytes, zstd-streamed). Receiver decodes into a new
     `DeviceIndex` and swaps the `Arc` in one step;
   - device no longer shared / gone → `Removed{device}`.
3. **Steady state**: every 60 s each owner sends new batches since the peer's acknowledged cursor (nothing when
   idle except a keep-alive). `SyncNow` (from either PC) makes every connected owner send immediately.
4. Receiver applies deltas with `DeviceIndex::apply` (id-based), updates the cursor, marks dirty, invalidates
   result sets, and `Ack{device, seq}`.
5. **Reconnect**: retry every 60 s and on mDNS announce; resumes from the stored cursors.
6. Only own devices are served (no relaying) in this spec.

**Speed targets** (new `seek perf` metrics): full copy of 2M entries end-to-end < 2 s on gigabit LAN; apply 1,000
changes < 50 ms; idle sync < 1 KB per peer per minute. Devices sync in parallel; search is never blocked.

## 6. Search, duplicates and access

- Remote devices are searched like local ones. `pc:<name>` filter added; `device:` also matches the PC name.
- **Same physical device on several PCs** (same `DeviceIdentity`: volume serial + filesystem, or normalised UNC):
  only the most recently scanned copy is searched; a currently connected local device always wins. The device list
  shows it once ("Backup 2TB · last seen on Laptop"); hidden copies are kept.
- **Access** for remote devices: elevated caller → everything. Otherwise hide the owner's profile folders except
  those of remote SIDs linked to the caller's SID. Reuses `hidden_mask` (keyed by device + caller SID).
  Network shares pushed by one user become visible to others only via this rule (closes the known gap for copies).
- Results from another PC: menu shows "On <PC> — not on this PC" and **Copy path**; Open/Reveal disabled.

## 7. UI

- Sidebar: *This PC*, then a group per linked PC (online dot, "synced 20 s ago", menu: Sync now, Rename, Unlink).
  A **Sync now** button at the top of the sidebar syncs all.
- Settings → **Linked PCs**: Link a PC (show code / pick PC + enter code), Share this PC's files switch, peer list.
- Own device menu: **Don't share with other PCs**.
- Results Device column: "Laptop › D:". Status bar: "3 PCs · synced 20 s ago".
- Unlink: confirm → delete that PC's copies, notify the peer (it removes us too).

## 8. Errors

| Situation | Behaviour |
|---|---|
| Peer offline | its devices offline with last-synced time; retry every 60 s and on mDNS announce |
| Drop during full copy | old copy kept; restarted next time |
| Corrupt payload / decode failure | discard that device's copy, request full copy, log |
| Reinstalled peer (new key) | refuse, peer `NeedsRelink` in UI |
| Wrong code ×3 / expired | code invalid; generate a new one |
| Journal damaged | new generation → peers get one full copy |
| Protocol version mismatch | no sync, "Update Seek on <PC>" |
| Group full (5) | pairing refused with message |

## 9. Testing

- `seek-sync` unit tests: journal numbering, trimming, reload after restart; delta-vs-full decision; full copy
  round trip; pairing (right code, wrong code, expiry, attempts); Noise refuses unknown keys; group join.
- Service integration test: two services in one process with fake volume sources over loopback — pair, full copy,
  search on B finds A's files, change on A + SyncNow → delta only, restart B → resumes with delta, full rescan on A →
  full copy, duplicate USB stick shows once, access rule on remote devices, unlink on both sides.
- `seek perf`: full-copy time/size (2M), delta apply (1,000), idle sync bytes.
- Manual: two real PCs on the user's LAN.

## 10. Later

Self-hosted server transport (same protocol, server relays/stores), relaying devices between peers, open/download
remote files, macOS/Linux service, more than 5 PCs.

# Seek Hub (self-hosted)

An always-on place your PCs sync through when they are not on the same network. It stores each PC's index
(encrypted at rest) and hands every PC the other PCs' drives. It is a small static binary in a `scratch` image,
reachable **only** through Tailscale Funnel at `https://seek-hub.<tailnet>.ts.net` (TLS 1.3, port 443). First
target: a Raspberry Pi 4 (64-bit OS with Docker). Design: `docs/superpowers/specs/2026-10-24-seek-hub-design.md`.

Files: `Dockerfile`, `compose.yaml` (Tailscale sidecar + hub), `funnel.json` (the one thing Funnel publishes),
`tailnet-policy.example.hujson`, `.env.example`. Runtime folders (git-ignored): `data/`, `keys/`, `tailscale/`.

## 1. Tailscale account (once)

1. Admin console -> DNS: turn on **MagicDNS** and **HTTPS Certificates**.
2. Admin console -> Access controls -> JSON editor: merge `tailnet-policy.example.hujson`. It defines `tag:seek-hub`
   and grants the `funnel` attribute to that tag. The default allow-all rule (`{"src":["*"],"dst":["*"],"ip":["*"]}`,
   or `{"action":"accept","src":["*"],"dst":["*:*"]}` in older files) must be **replaced** by the file's two rules
   (do not just delete it: that would cut your own devices off from each other). Your own devices keep reaching
   each other, the hub can not reach them (tagged nodes are not "members"), and they can reach the hub only on port
   443, its one published port. That rule is what lets a PC with Tailscale running connect: its DNS gives the hub's
   tailnet address instead of Funnel. Also remove any broader `funnel` nodeAttr (one targeting `*` or
   `autogroup:member`). Set your own login as the tag owner if you are not an admin.
3. Settings -> Keys -> **Generate auth key**: reusable off, ephemeral off, tag `tag:seek-hub`. Single use is enough.
   Tagged nodes do not expire.
4. `cp .env.example .env`, then fill in `TS_AUTHKEY` and `SEEK_HUB_PUBLIC=tls://seek-hub.<tailnet>.ts.net`
   (your tailnet name is shown on the DNS page). The auth key is only needed for the first start. Blank it only
   after the first start worked and `tailscale/` holds the node state; blanking it earlier leaves the sidecar
   unable to join.

**A value in `.env` overrides what `init` saved** (`SEEK_HUB_PUBLIC` and `SEEK_HUB_LAN`; the name and quotas come
only from `init`). Leave a line empty to use `init`'s value.

## 2. Build (on the Windows dev machine, no Docker needed)

```powershell
.\scripts\build-hub-image.ps1        # writes docker\dist\arm64\seek-hub and docker\dist\amd64\seek-hub
```

It checks for `zig`, `cargo-zigbuild` and the two rustup targets (and prints how to install what is missing; after
`winget install zig.zig` refresh `$env:Path` or open a new terminal), runs `cargo deny` / `cargo audit` when
installed, and cross-compiles `seek-hub` for `aarch64-` and `x86_64-unknown-linux-musl`. The image itself is built
on the Pi (next section), because `docker/dist/` is git-ignored and must be copied:

```powershell
scp -r docker pi@<pi>:seek-hub
```

This creates `~/seek-hub` on the Pi with the folder's contents (the Dockerfile at `~/seek-hub/Dockerfile`), as
long as `~/seek-hub` does not exist yet. To update later, copy just the new binary:
`scp docker/dist/arm64/seek-hub pi@<pi>:seek-hub/dist/arm64/`. The Dockerfile marks the binary executable itself
(`COPY --chmod`, needs Docker 23 or newer, which the `get.docker.com` install gives you).

Optional, if you do have Docker on your PC: `-Load arm64` also builds the image and writes
`docker\seek-hub-arm64.tar` (copy it to the Pi and `docker load -i` it); `-Push` publishes
`ghcr.io/calinasandu/seek-hub:<version>` and never runs unless you pass it.

## 3. Set up and run (on the Pi)

```sh
cd ~/seek-hub
# 1. build the image (the tag must match compose.yaml, or compose tries to pull it)
docker build --build-arg TARGETARCH=arm64 -t ghcr.io/calinasandu/seek-hub:0.1.0 .

# 2. folders: the hub runs as user 10001, so give it data/ and keys/. The sidecar is root with every capability
# dropped, so it can only use a folder root owns
mkdir -p data keys tailscale && sudo chown 10001:10001 data keys && sudo chown root:root tailscale

# 3. the key (once). The hub is not running yet, so mount /keys writable for this one command
docker run --rm -v "$PWD/data:/data" -v "$PWD/keys:/keys" ghcr.io/calinasandu/seek-hub:0.1.0 init --keys
```

`/keys` is mounted read-only while the hub runs, which is why the key is created with a one-off writable mount. If
the key is missing the hub refuses to start (exit 2) and prints this command. Because `keys/` now belongs to user
10001, copying or backing up `keys/hub.key` needs `sudo` (`sudo cp keys/hub.key <somewhere safe>`). `data/` and
`keys/` must be on a Unix filesystem (the hub uses sockets and `flock`): not exFAT/NTFS on a USB stick, use ext4.

**`init`** (hub name, quotas, optional LAN address; prints the Tailscale checklist and the public address) writes
the store and needs the data directory to itself, so run it only while the hub is stopped. It is optional, the
defaults work. Run it only after the first `docker compose up -d`: the hub shares the tailscale container's
network, so that container must exist. The one-shot form is `docker compose run --rm --no-deps seek-hub <command>`
(the image's entrypoint is the binary, so do not repeat `seek-hub`):

```sh
docker compose stop seek-hub; docker compose run --rm --no-deps seek-hub init; docker compose start seek-hub
```

Running it against a live hub is refused ("the hub is running ... Stop it while you run init"). Start everything
with `docker compose up -d`. All other commands talk to the running hub through `docker exec` (the image has
`PATH=/`, so `seek-hub` resolves):

```sh
docker exec -it seek-hub seek-hub link      # opens pairing for 2 min: 8-digit code + addresses
docker exec seek-hub seek-hub status        # PCs, last seen, stored drives and sizes, disk use, refused handshakes
docker exec seek-hub seek-hub pcs
docker exec seek-hub seek-hub unlink <pc>
docker exec seek-hub seek-hub doctor --public
```

In the Seek app: *Link a device -> Hub*, paste the address, type the code. A PC that links to the hub drops its
direct LAN links (without telling those peers, so they keep its copies; it keeps its copies of their drives until
the hub delivers fresher ones).

## Reading `seek-hub status`

One entry per linked PC (name, last seen, drives stored with their sizes), then the disk use of `/data`, the
counters (refused handshakes, ownership violations, quota hits) and the public address. Refused handshakes rising
without you linking anything means strangers are probing the public endpoint; the pairing window is closed unless
`link` is running, so they get nothing. `doctor` checks the key, that the store decrypts, the listener, free disk
and (with `--public`) that Funnel answers with a valid TLS 1.3 certificate for the right host.

## Where the key lives (trade-off)

`keys/hub.key` encrypts everything in `data/`. If both sit on the same SD card the encryption only stops casual
reading of a lost card; it does not help against someone who takes the whole Pi. Keep `keys/` on a different device
(a USB stick formatted ext4 and mounted at `./keys`) if that matters to you. Without the key the stored data is
unreadable: the fix is to wipe `data/` and link the PCs again (the PCs are the truth and upload again).

## Rotating the key

`rotate-key` re-encrypts everything under a new key. The running hub cannot do it: `/keys` is mounted read-only
there, and the command would fail asking for a writable mount. Run it with the hub stopped:

```sh
docker compose stop seek-hub
docker run --rm -v "$PWD/data:/data" -v "$PWD/keys:/keys" ghcr.io/calinasandu/seek-hub:0.1.0 rotate-key
docker compose start seek-hub
sudo cp keys/hub.key <somewhere safe>      # back up the NEW key; the old backup no longer decrypts data/
```

It writes `keys/hub.key.new`, re-encrypts every file, then replaces `hub.key`. If it is interrupted (power loss,
error), run the same `docker run ...` command again: it finishes the unfinished rotation. Do not start the hub
with `keys/hub.key.new` present and unfinished unless you must; it finishes the rotation itself at start but can
only replace `hub.key` when `/keys` is writable. Linked PCs are closed during the rotation and reconnect on their
own.

## Upgrade and backup

```sh
# published image
docker compose pull && docker compose up -d
# image built on the Pi: copy the new docker/ folder over, then
docker build --build-arg TARGETARCH=arm64 -t ghcr.io/calinasandu/seek-hub:0.1.0 . && docker compose up -d
```

Backup: copy `data/` **and** `keys/`, separately (a copy of `data/` alone cannot be read; use `sudo`, both belong
to user 10001). `tailscale/` holds the node identity; losing it only means creating a new auth key.

## Funnel configuration

`funnel.json` is a Tailscale serve config: TLS-terminated TCP **443 -> 127.0.0.1:47321** with `AllowFunnel` for
`${TS_CERT_DOMAIN}:443` (the container substitutes the placeholder). Nothing else is published. The equivalent CLI
is `tailscale funnel --bg --tls-terminated-tcp=443 tcp://127.0.0.1:47321`. If a future Tailscale image rejects the
file, run that command in the sidecar instead (`docker compose exec tailscale tailscale funnel --bg ...`).
Optional LAN access: uncomment `ports` in `compose.yaml` and set `SEEK_HUB_LAN=1` in `.env` (or via `init`).

## Checkpoint on the Pi (what to run, in order)

On the Windows PC:

```powershell
.\scripts\build-hub-image.ps1
scp -r docker pi@<pi>:seek-hub
```

On the Pi (`cd ~/seek-hub`):

```sh
docker build --build-arg TARGETARCH=arm64 -t ghcr.io/calinasandu/seek-hub:0.1.0 .
mkdir -p data keys tailscale && sudo chown 10001:10001 data keys && sudo chown root:root tailscale
cp .env.example .env && nano .env                       # TS_AUTHKEY and SEEK_HUB_PUBLIC
docker run --rm -v "$PWD/data:/data" -v "$PWD/keys:/keys" ghcr.io/calinasandu/seek-hub:0.1.0 init --keys
sudo cp keys/hub.key <somewhere safe>
docker compose up -d
# optional (name, quotas, LAN). After `up`, because the hub shares the tailscale container's network
docker compose stop seek-hub; docker compose run --rm --no-deps seek-hub init; docker compose start seek-hub
docker compose logs --tail 30 tailscale seek-hub        # tailscale joined, hub listening, no errors
# Funnel serves a valid certificate (handshake only, no request is sent). The certificate and DNS name can take
# a few minutes after the first start: if this fails, wait and retry.
openssl s_client -connect seek-hub.<tailnet>.ts.net:443 -servername seek-hub.<tailnet>.ts.net -tls1_3 </dev/null 2>&1 \
  | grep -E "Verify return code|Protocol|subject="              # expect: 0 (ok), TLSv1.3
docker exec seek-hub seek-hub doctor --public
```


Then link two PCs: PC A on your home network, PC B on a **phone hotspot with Tailscale off**. On the Pi run
`docker exec -it seek-hub seek-hub link`; in each PC's Seek: *Link a device -> Hub*, address
`tls://seek-hub.<tailnet>.ts.net`, the 8-digit code (run `link` again for the second PC). Search for a file that
lives on the other PC, then:

```sh
docker exec seek-hub seek-hub status     # shows both PCs, their drives and sizes
```

## Memory and the `mem_limit: 1g` in compose

Compaction (folding a drive's journal into its full copy) is the hub's memory peak: it decodes the whole copy.
Measured with `seek perf` (release build, 2,000,000 entries + 10,000 changes, whole process): **about 513 MB
peak working set** (about 260 bytes per entry; it was 583 MB before the final fix pass), taking 1.5 s. Add the
rest of the process (runtime, a session being served: up to the PC's quota, at most 256 MiB per frame) and
`mem_limit: 1g` holds comfortably for drives up to **about 2 million entries** each (about 3 million is the
absolute edge for compaction alone). Compaction runs one drive at a time. For larger drives (or a Pi with
more RAM), raise `mem_limit` in `compose.yaml` in proportion: roughly 300 MB per million entries of the biggest
drive, plus 300 MB.

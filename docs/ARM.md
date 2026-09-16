# Running on an ARM Linux board

The server needs Linux, writable configuration/data directories, and access to your
library. Use the target matching the board's **operating system**, not just its CPU:
a 64-bit CPU can run a 32-bit distribution. Check `uname -m` and `getconf LONG_BIT`.

- 64-bit ARM (`aarch64`, 64-bit userland): `aarch64-unknown-linux-musl`.
- ARMv7 hard-float (`armv7l`, 32-bit userland): `armv7-unknown-linux-musleabihf`.
- ARMv6 hard-float (`armv6l`, 32-bit userland): `arm-unknown-linux-musleabihf`.

These are Linux targets, not bare-metal firmware. Soft-float distributions need
a different target/toolchain. The existing container workflow builds all three
architectures; a native build or a copied static binary avoids needing Docker on
small boards.

## Native build (ARM64 or ARMv7)

Install Rust 1.92 or newer, a C compiler, and the system linker. SQLite is bundled;
Zstandard compression is Rust code. On Debian/Ubuntu, install `build-essential`,
`pkg-config`, and `ca-certificates`, then install Rust using your normal Rust
installation method.

From this checkout:

```sh
cargo build --release --locked -p ownfoil-rs -j 1
mkdir -p config data
OWNFOIL_INSECURE_ADMIN_COOKIE=true ./target/release/ownfoil-rs \
  --bind 0.0.0.0:8465 \
  --library-folder /mnt/games \
  --settings ./config/settings.yaml
```

Open `http://BOARD_IP:8465` and create the first administrator. The cookie override
is for direct plain HTTP access; omit it when using HTTPS. Native state defaults
to `./data`, relative to the working directory. Keep that directory stable across
restarts. Upload your console keys through Settings when using verification or
conversion.

`-j 1` limits build concurrency, not runtime workers. For ARMv6, cross-build on a
larger computer because Rust host toolchains are not available for every ARMv6 OS.

## Cross-build a static binary

On a Linux development machine with Docker running and `cross` installed:

```sh
# Replace the target for ARMv7 or ARMv6 as listed above.
cross build --release --locked --target aarch64-unknown-linux-musl -p ownfoil-rs
scp target/aarch64-unknown-linux-musl/release/ownfoil-rs user@BOARD_IP:~/ownfoil-rs
```

On the board, create `config` and `data`, make the binary executable, and run it
with the same arguments as the native example. A musl binary does not require
Rust or a matching glibc on the board. Install CA certificates for HTTPS TitleDB
requests. The build used by CI is recorded in
[container.yml](../.github/workflows/container.yml).

## Containers

On ARM64 or ARMv7, the checked-out version can be built with the existing Compose
file:

```sh
cp .env.example .env
# Set GAMES_PATH, PUID/PGID, and the cookie setting for your deployment.
docker compose up --build -d
```

For ARMv6, cross-build the binary first and package it with `Dockerfile.cross`;
the native `Dockerfile` uses Debian images that do not provide ARMv6. The
multi-architecture publishing workflow already uses this cross-build path. If
using a published image instead, select a tag containing the desired changes;
`v0.3.0` does not include unmerged branch work.

## Runtime settings for a small board

In Settings, start with two workers and an I/O concurrency limit of one. Leave
automatic compression disabled until normal serving works. When enabling it,
start with one compression thread, level 1, and a block exponent of 20 (1 MiB).
Larger blocks and high compression levels increase memory/CPU use. The 128 MiB
block batching budget covers input buffers, not the encoder's total memory.
Solid compression currently uses one encoder thread.

Keep the SQLite database and TitleDB cache on local storage. Allow free space
for the original file plus converted output; conversion verifies output before
removing the source. A read-only games mount supports serving, but not organizing,
converting, or deleting files. Network filesystems may need periodic scans if
filesystem notifications are unavailable.

For a native systemd service, install the binary to `/usr/local/bin/ownfoil-rs`,
create an unprivileged `ownfoil` account, and give it access to `/mnt/games` and
write access to `/var/lib/ownfoil/config` and `/var/lib/ownfoil/data`. Example unit:

```ini
[Unit]
Description=Ownfoil Rust library server
Wants=network-online.target
After=network-online.target

[Service]
User=ownfoil
Group=ownfoil
WorkingDirectory=/var/lib/ownfoil
ExecStart=/usr/local/bin/ownfoil-rs --bind 0.0.0.0:8465 --library-folder /mnt/games --settings /var/lib/ownfoil/config/settings.yaml
Restart=on-failure
KillSignal=SIGINT
# Enable only for direct plain HTTP administration:
# Environment=OWNFOIL_INSECURE_ADMIN_COOKIE=true

[Install]
WantedBy=multi-user.target
```

After installing the unit as `/etc/systemd/system/ownfoil.service`:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now ownfoil
curl --fail http://127.0.0.1:8465/health
journalctl -u ownfoil -f
```

Validate one library scan, one client download, and restart persistence on the
actual board before enabling automatic file management. Cross-compilation proves
build compatibility; it does not measure board memory use or Switch-client behavior.

# Cross-compile for Linux VPS

The gateway ships as a single fully static `musl` binary — no libc, no runtime
deps on the VPS. Two ways to build it from a Windows/macOS dev machine.

## Option A — Docker (recommended, no host toolchain)

Requires only a running Docker daemon. From the repo root:

```bash
bash server/deploy/build-linux.sh
# or directly:
docker run --rm -v "$PWD/server:/build" -w /build rust:1.99-alpine \
  sh -c "apk add --no-cache musl-dev && rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl"
```

Output: `server/target/x86_64-unknown-linux-musl/release/antigravity-server`
(static ELF, ~8–12 MB). For ARM VPS set `TARGET=aarch64-unknown-linux-musl`.

PowerShell equivalent:

```powershell
docker run --rm -v "${PWD}\server:/build" -w /build rust:1.99-alpine `
  sh -c "apk add --no-cache musl-dev; rustup target add x86_64-unknown-linux-musl; cargo build --release --target x86_64-unknown-linux-musl"
```

## Option B — cargo-zigbuild (no Docker)

```bash
pip install ziglang
cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl --manifest-path server/Cargo.toml
```

## Verify the binary is static

```bash
file server/target/x86_64-unknown-linux-musl/release/antigravity-server
# ELF 64-bit LSB pie executable, x86-64, static-pie linked, ...
readelf -l .../antigravity-server | grep INTERP   # no output = no dynamic loader
```

A `static-pie linked` ELF (musl bundled, no `INTERP` segment) runs on any
x86_64 Linux (Debian/Ubuntu/Alpine/RHEL) with no shared-library requirements.
Verified: the built binary serves `GET /health` → `{"status":"ok"}` inside a
bare Alpine container.

## Deploy

```bash
scp server/target/x86_64-unknown-linux-musl/release/antigravity-server user@vps:/opt/antigravity-server/
scp server/config.example.toml user@vps:/opt/antigravity-server/server.toml
scp server/deploy/antigravity-server.service user@vps:/etc/systemd/system/
# edit server.toml + /etc/antigravity-server.env on the VPS, then:
ssh user@vps 'sudo systemctl daemon-reload && sudo systemctl enable --now antigravity-server'
```

See the main [README](../README.md) for configuration, OAuth credential
extraction (`tools/extract-creds.py`), and the reverse-proxy/SSE caveats.
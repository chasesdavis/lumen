# Lumen

An agent-native encrypted virtual drive. Mounts as a normal volume on macOS,
Linux, and Windows while exposing first-class MCP tools so agents read, search
and write files directly instead of scraping a Finder window.

Built as a faster, cheaper, more portable answer to Helumi (macOS 26.2+ /
Finder-only / USD 10 flat):

| | Helumi | Lumen |
|---|---|---|
| Platforms | macOS 26.2+ only | macOS 12+, Linux, Windows |
| Core | native Swift | shared Rust core, thin FUSE/WinFsp shims |
| Chunking | fixed-size, whole-tail re-uploads | FastCDC, edits cost only changed chunks |
| Cache | LRU bytes only | hit/miss accounting + learning prefetcher |
| Agents | files visible in Finder | MCP server with list/search/read tools |
| Storage | Cloudflare R2 locked | pluggable S3-compatible: R2, B2, Storj, BYOB |
| Price | USD 10/mo flat | USD 3/mo base + storage pass-through at cost + 10% |

## Architecture

- `lumen-core` - chunking, AES-256-GCM envelope encryption (HKDF per-chunk
  keys, AAD-bound to fingerprints), SQLite metadata index, ChangeLog sync,
  cache, prefetcher, GC, usage metering
- `lumen-fsprobe` - VFS layer over any `ObjectStore` backend
- `lumen-fs-adapter` - POSIX-shaped mount operations shared by all platforms
- `lumen-fuse-shim` / `lumen-winfsp-shim` - thin platform callbacks (~500 LOC each)
- `lumen-mcp-server` - stdio/TCP MCP tools over the encrypted index
- `lumen-api-server` - Axum auth, sync, Dodo Payments billing, GC admin
- `lumen-storage-adapters` - S3-compatible store (R2/B2/Storj/MinIO/BYOB)

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

macOS DMG: `./scripts/build-macos-dmg.sh` (macFUSE is a runtime dependency:
`brew install --cask macfuse`). Add `LUMEN_UNIVERSAL=1` for an arm64 + x86_64
universal build. Windows ZIP: `scripts/build-windows-zip.ps1` on a Windows
host (WinFsp SDK required).

## Run

```bash
# API server (auth + billing + sync). Env: LUMEN_DODO_API_KEY,
# LUMEN_DODO_WEBHOOK_KEY, LUMEN_DODO_MODE, LUMEN_DODO_BASE_PRODUCT_ID
cargo run -p lumen-api-server

# MCP server over the local encrypted index
cargo run -p lumen-mcp-server -- --transport stdio --index-path ./lumen.db

# Mount a drive against a local store directory (dev mode)
cargo run -p lumen-fuse-shim --bin lumen-mount -- \
  --mountpoint /Volumes/Lumen --index-path ./lumen.db \
  --store-endpoint file://./store --bucket chunks
```

## Security model

Files are chunked, encrypted client-side, and uploaded to whatever S3
backend is configured. The master key never leaves the client; per-chunk
keys derive via HKDF and are bound to chunk fingerprints through AES-GCM
AAD, so substitution across chunks fails closed. The service sees ciphertext
and sizes only.

## Status

Early beta. Test suite: 110 passing across chunking determinism, crypto
round-trips, concurrent index writes, live MinIO S3 contracts, and
Xcode/Docker/video workload patterns. See `AGENTS.md` for contributor
commands and conventions.

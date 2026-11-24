## Overview

Lumen is a cross-platform, agent-native virtual drive (Helumi competitor). Rust workspace: shared core crate (lumen-core) for chunking, encryption, sync, and caching; storage adapters (lumen-storage-adapters) abstracting S3-compatible backends (R2/B2/Storj/BYOB); filesystem adapter (lumen-fsprobe) wrapping FUSE-T/WinFsp; MCP server (lumen-mcp-server) exposing tools for AI agents; and an Axum API server (lumen-api-server) for auth, accounts, billing, and metadata.

## Commands

If no cargo in PATH, source $HOME/.cargo/env first.

| Task | Command |
| --- | --- |
| Setup | cargo build --workspace |
| All tests | cargo test --workspace |
| Core only | cargo test -p lumen-core |
| Lint | cargo clippy --workspace |
| Release build | cargo build --release --workspace |
| Run MCP server (stdio) | cargo run -p lumen-mcp-server |
| Run MCP server (TCP + persisted index) | cargo run -p lumen-mcp-server -- --transport tcp --listen 127.0.0.1:9317 --index-path /path/to/index.db |
| Mount drive (Unix) | cargo run -p lumen-fuse-shim --bin lumen-mount -- --mountpoint /Volumes/Lumen --index-path ~/lumen.db --store-endpoint https://api.cloudflare.com/client/v4/accounts/<ACCOUNT>/r2 --bucket lumen-chunks |
| Mount drive (Windows) | cargo run -p lumen-winfsp-shim --bin lumen-mount-winfsp -- --mountpoint C:\Lumen --index-path <INDEX_DB> --store-endpoint file://<STORE_DIR> --bucket lumen-chunks |
| macOS distribution bundle + DMG | ./scripts/build-macos-dmg.sh (add SIGN_IDENTITY="Developer ID Application: <NAME>" to sign; artifacts land in dist/, which is gitignored) |
| Universal (arm64 + x86_64) DMG | LUMEN_UNIVERSAL=1 ./scripts/build-macos-dmg.sh (lipo-joins both slices; needs the x86_64-apple-darwin rustup target, installed automatically) |
| macOS bundle only, no DMG | ./scripts/build-macos-dmg.sh --no-dmg |
| Windows portable ZIP | ./scripts/build-windows-zip.sh (cross-compiles from macOS/Linux via cargo-xwin; needs `rustup target add x86_64-pc-windows-msvc`, `cargo install cargo-xwin`, and `llvm-lib` on PATH — `brew install llvm` on macOS). Native Windows hosts can use powershell scripts/build-windows-zip.ps1 instead. |
| Live S3 contract (MinIO) | minio server --address 127.0.0.1:9199 --certs-dir /tmp/minio-certs /tmp/minio-data & then `mc mb local/lumen-test` with alias set to that endpoint, finally MINIO_TEST_ENDPOINT=http://127.0.0.1:9199 cargo test -p lumen-storage-adapters --test s3_contract. Without the env var the live layer skips silently and only the in-memory layer runs. Plaintext http:// endpoints are allowed by S3Store automatically. |

## Conventions

- Pure Rust workspace; single shared lumen-core. Platform FS adapters stay thin (~500 LOC max) so edge cases do not leak into core. Platform-specific logic lives behind traits defined in core and consumed by adapter crates.
- Storage layer is trait-based; each provider implements ObjectStore (async trait, S3-shaped). Error types use thiserror; crypto always fails closed.
- Server-side mutations always go through lumen_core::changelog::ChangeLog (monotonic seqs). SyncEngine consumes Arc<ChangeLog> via the core ChangeFeed impl; never hand out raw cursors outside it.
- Write-path commits flow through CommitBatcher (lumen_core::upload), never bare index.upsert-to-changelog hops: MountOps create/mkdir/replace upsert into MetadataIndex immediately (lookup-visible, no FUSE miss) then queue the RemoteItem in CommitBatcher; platform adapters call commit_pending() at operation boundaries so N file syncs commit as one ChangeLog::append_batch lock acquisition. NEVER flush per-file and never bypass the batcher in new write flows.
- Billing metering flows through lumen_core::meter::UsageMeter only. VfsOps reports stored bytes on fresh uploads (ciphertext size) and retrieved bytes on cache misses; lumen_api_server::usage::ledger_sink binds a meter to a per-user UsageLedger. Never record usage anywhere but through these two seams.
- Dodo billing lives in lumen_api_server::dodo + dodo_client. Webhooks follow the Standard Webhooks spec: three headers (webhook-id, webhook-timestamp, webhook-signature), HMAC-SHA256 over "{id}.{timestamp}.{raw_body}", Base64-encoded with "v1," prefix, space-separated candidates for key rotation, 5-minute replay tolerance. Checkout sessions POST to /checkouts with product_cart; usage billing POSTs meter events to /events/ingest where event_id is the idempotency key (derive deterministically as "{customer_id}_storage_{period}"). Env: LUMEN_DODO_API_KEY, LUMEN_DODO_WEBHOOK_KEY. Never verify signatures with string equality or trust event bodies without the signature check.
- Checkout fulfillment is metadata-first: POST /v1/billing/checkout embeds {"user_id": claims.sub} in the Dodo session metadata (authenticated JWT at creation time). The /v1/dodo/webhook handler checks data.metadata.user_id BEFORE data.customer_id: link_and_grant_by_user_id grants the plan and persists customer_id in one UPDATE (the authoritative path), grant_plan_by_dodo_customer remains only as the fallback for pre-metadata subscriptions/renewals. Checkout 501s when LUMEN_DODO_API_KEY or LUMEN_DODO_BASE_PRODUCT_ID are unset. NEVER trust user_id without signature verification; NEVER delete the customer_id fallback.
- Content search: MCP search_files kind=content queries MetadataIndex.search_content over the local plaintext body index (content_index table). Agents opt files in via the index_content tool; nothing indexes content implicitly.
- MCP auth: with LUMEN_JWT_SECRET set, tool calls are gated behind the "authorize" tool (validates an Access-scope JWT against the same KeyMaterial as the api-server; connection stays authorized after one success). Unauthenticated tool calls return JSON-RPC error -32001. Without LUMEN_JWT_SECRET the server runs in open mode and "authorize" fails loudly rather than pretending tokens were verified. Never bypass session.rs gating logic in future transports.
- MCP write path (create_file/replace_file/delete_file) requires Tools::with_commit_log (storage + changelog mode); index-only Tools report a typed limitation, never a panic. create_file takes flat names only (no "/" - parent_id carries hierarchy), server-generates ids, caps writes at 1 MiB per call; replace_file bumps content_version AND metadata_version; delete_file removes from index immediately but commits its tombstone through CommitBatcher::removal which bypasses the queue - tombstones must never wait behind upserts or a cursor follower can resurrect deleted items from a stale page.
- Unix mounting lives in lumen-fuse-shim: FuseShim implements fuser::Filesystem over Arc<MountOps> with u64-inode -> path translation in the shim only (UUIDs stay canonical everywhere else). Sync FUSE callbacks bridge to async via block_on (block_in_place inside multi-thread runtime, futures polling otherwise) - same discipline as MCP vfs_read_blocking. Removals map unlink AND rmdir onto MountOps::remove; both go straight to CommitBatcher::removal for immediate tombstones. Windows ships a WinFsp binary later; NEVER let platform code call VfsOps/MetadataIndex directly - everything routes through MountOps so batching and metering stay shared.
- Windows mounting lives in lumen-winfsp-shim (NOT gated behind cfg-only code that skips review on macOS): portable path translation lives in src/path.rs and runs in the normal workspace test suite on every platform; src/error.rs maps Stat -> FileInfo and AdapterError -> FspError. The #[cfg(windows)] FileSystemContext impl follows winfsp-rs conventions: deletes are DEFERRED (set_delete validates only; cleanup does the real remove via FspCleanupDelete flag), read_directory uses the DirMarker restart protocol (never offset-based like FUSE), FileContext is the POSIX path string, VolumeParams set file_info_timeout=u32::MAX, unicode_on_disk=true, flush_and_purge_on_cleanup=true. Cross-compile validation from macOS is NOT possible today (winfsp-sys bindgen needs Windows SDK headers); correctness is enforced via path.rs unit tests plus careful API conformance against docs.rs sources - run cargo test -p lumen-winfsp-shim after any shim change.
- removal semantics across surfaces: FS remove() and MCP delete_file leave encrypted chunks in storage until garbage collection reclaims unreferenced keys (GC milestone pending). Do not add eager chunk deletion on these paths.
- storage GC (lumen_core::gc) is mark-and-sweep over CHUNK_KEY_PREFIX (chunking.rs): gc_collect reads the live reference set from MetadataIndex.referenced_chunk_keys, lists the chunk prefix on ObjectStore, deletes unreferenced keys. NEVER delete eagerly from FS remove() or MCP delete_file. NEVER run gc_collect while any CommitBatcher feeding that index holds unflushed upserts - write_full uploads chunks before callers queue their metadata upsert, so unflushed content looks like an orphan and would be swept. The MCP gc_run tool flushes its own CommitBatcher first and reports pending_commits_flushed; every new GC entry point must honor the same flush-first contract.
- GC reclaim bytes flow through UsageMeter via gc_and_report (reclaimed ciphertext size in stored_bytes of one UsageEvent). Reclaimed bytes are reported only when non-zero so idle sweeps generate no billing noise.
- API-server GC admin surface: AppState carries an optional GcBackend { Arc<MetadataIndex>, Arc<dyn ObjectStore> }. When present, /v1/files and /v1/files/delete also feed the server-side index (chunk keys + metadata only, plaintext never reaches the server). POST /v1/admin/gc runs gc_collect and records reclaimed bytes as a NEGATIVE stored-bytes delta on the JWT subject's ledger row - reclaims must reduce pass-through billing, never inflate it. Absent backend answers 501 (pure changelog mode). Config: LUMEN_INDEX_DB enables it, then LUMEN_LOCAL_STORE_DIR or LUMEN_S3_ENDPOINT+LUMEN_S3_BUCKET select the store; S3 credentials always come from env. NEVER attach an index without a store - reference tracking silently degrades into false sweeps.
- LocalObjectStore (lumen_fsprobe::local) is a file-backed ObjectStore for dogfooding/dev mounts and cross-process tests; it honors the same contract as S3Store incl. typed NotFound, idempotent delete, prefix-scoped recursive list sorted by key. lumen-mount --store-endpoint file:// routes to it; anything else requires LUMEN_S3_ACCESS_KEY_ID/LUMEN_S3_SECRET_ACCESS_KEY env vars via S3Store::from_env. NEVER read S3 credentials from argv.
- Naming: crates are lumen-domain, modules follow crate::domain::name.
- VFS reads feed both cache hits and misses into the Prefetcher as access
  signals; never gate observation on miss-only paths.

Example:

```rust
#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put(&self, key: &str, body: Bytes) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Bytes>;
    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>>;
    async fn delete(&self, key: &str) -> Result<()>;
}
```


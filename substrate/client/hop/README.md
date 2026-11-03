# `sc-hop` — Hand-Off Protocol

Node-level ephemeral data pool for Substrate collators. HOP gives a collator a
disk-backed pool where an authorized sender can upload a blob and hand it off
to one or more recipients who claim it directly from the same collator over
JSON-RPC. Unclaimed blobs are promoted to on-chain storage as a best-effort
fallback before they expire, or simply cleaned up.

HOP keeps short-lived hand-off data off-chain until it actually needs
permanence — data lives on one collator's disk instead of being replicated
across the chain, and round-trip latency stays well under a block time. The
node is agnostic about what "authorized to submit" and "promote on-chain"
mean; both are delegated to the runtime via the `sp_hop::HopRuntimeApi`.

## Overview

- **Disk-backed** — blobs are written to disk immediately, only metadata lives
  in RAM. The in-memory index is rebuilt from on-disk `.meta` files on restart.
- **Content-addressed** — entries are keyed by `blake2_256(data)`; duplicates
  are rejected at submit time.
- **Per-recipient ephemeral keypairs** — the sender generates a one-time
  `MultiSigner` keypair per recipient and shares the private key out-of-band.
  The collator verifies signatures on claim/ack without learning recipient
  identities.
- **Domain-separated signatures** — distinct context prefixes for submit,
  claim, and ack (`HOP_SUBMIT_CONTEXT`, `HOP_CLAIM_CONTEXT`, `HOP_ACK_CONTEXT`)
  so a signature from one operation cannot be replayed as another. Submit
  signatures also bind `submit_timestamp` so an old `(data, signer, signature)`
  cannot be replayed indefinitely.
- **Runtime-defined limits and authorization** — per-submission size cap comes
  from `HopRuntimeApi::max_promotion_size` (authoritative, no separate node
  ceiling); per-account authorization is gated by
  `HopRuntimeApi::can_account_promote`, so the runtime decides what
  "authorized" means (e.g. reuse an existing on-chain authorization, check a
  dedicated HOP allowlist, or any other policy).
- **Per-account rate limiting** — token-bucket caps on both submit rate and
  bandwidth; see [CLI flags](#cli-flags).
- **Best-effort on-chain promotion** — near-expiry entries are promoted via a
  runtime API; if the runtime doesn't implement `HopRuntimeApi` the node runs in
  cleanup-only mode so it can be deployed ahead of a runtime upgrade.

## Crate layout

| Module | Purpose |
|---|---|
| `cli` | `HopParams` — `clap`-flattenable CLI parameters |
| `pool` | `HopDataPool` — disk-backed blob store + in-memory metadata index |
| `metrics` | `HopMetrics` — Prometheus metrics; no-ops without a registry |
| `rpc` | `HopApi` / `HopRpcServer` — jsonrpsee methods (`hop_submit`/`claim`/`ack`/`poolStatus`) |
| `promotion` | `HopPromoter`, `HopMaintenanceTask`, `build_maintenance_task` — background promotion + cleanup |
| `rate_limit` | `RateLimitConfig`, `RateLimiter` — per-account token buckets |
| `types` | Errors, `HopEntryMeta`, `PoolStatus`, `SubmitResult`, signing contexts, defaults |

Companion runtime crate: [`sp-hop`](../../primitives/hop/) — defines the
`HopRuntimeApi` runtime API used for authorization checks and promotion.

## Integration

Three-step wiring for a Cumulus / omni-node style service builder.

### 1. Flatten CLI

```rust,ignore
use sc_hop::HopParams;

#[derive(Debug, clap::Parser)]
pub struct Cli {
    #[clap(flatten)]
    pub hop: HopParams,
    // ... other CLI fields
}
```

### 2. Initialize the pool

```rust,ignore
use sc_hop::{HopDataPool, HopMetrics};
use std::sync::Arc;

let hop_pool = hop_params.enabled.then(|| {
    HopDataPool::new(
        hop_params.max_pool_size * 1024 * 1024,  // pool cap, bytes
        hop_params.max_user_size * 1024 * 1024,  // per-user cap, bytes
        hop_params.retention_secs,
        hop_params.data_dir.clone()
            .unwrap_or_else(|| chain_data_dir.join("hop")),
        hop_params.rate_limit_config(),
        // Registration must not fail startup, so degrade to no-ops.
        HopMetrics::new(prometheus_registry).unwrap_or_else(|_| HopMetrics::disabled()),
    )
    .map(Arc::new)
    .map_err(|e| format!("Failed to create HOP pool: {e}"))
}).transpose()?;
```

`HopParams::build_pool(database_path, prometheus_registry)` does all of the above.

### 3. Register RPC and spawn the maintenance task

```rust,ignore
use sc_hop::{build_maintenance_task, HopApiServer, HopRpcServer};

if let Some(pool) = hop_pool.clone() {
    rpc_module.merge(HopRpcServer::<_, Block>::new(pool, client.clone()).into_rpc())?;
}

if let Some(pool) = hop_pool {
    let task = build_maintenance_task(
        &client,
        &transaction_pool,
        pool,
        hop_params.promotion_buffer_secs,
        hop_params.check_interval,
    );
    task_manager.spawn_handle().spawn("hop-maintenance", None, task.run());
}
```

`build_maintenance_task` detects `HopRuntimeApi` support at startup and falls back to
cleanup-only if the runtime doesn't implement it.

## CLI flags

| Flag | Default | Description |
|---|---|---|
| `--enable-hop` | off | Enable HOP |
| `--hop-max-pool-size <MiB>` | 10240 (10 GiB) | Total pool size cap |
| `--hop-max-user-size <MiB>` | 256 | Per-user hard cap (not scaled by active users) |
| `--hop-retention-secs <s>` | 86400 (24 h) | How long entries are kept before expiry |
| `--hop-check-interval <s>` | 300 | Maintenance cycle period |
| `--hop-promotion-buffer-secs <s>` | 7200 (2 h) | Seconds before expiry to start promoting |
| `--hop-submit-rate-per-min <n>` | 60 | Sustained per-account submit rate |
| `--hop-submit-burst <n>` | 120 | Per-account submit burst size |
| `--hop-bandwidth-per-min-mib <MiB>` | 128 | Sustained per-account bandwidth |
| `--hop-bandwidth-burst-mib <MiB>` | 256 | Per-account bandwidth burst |
| `--hop-disable-rate-limit` | off | Disable per-account rate limiting (dev/tests only) |
| `--hop-data-dir <path>` | `<chain-data-dir>/hop` | Directory for persistent blob and metadata storage |

All HOP RPC methods are also subject to the node-global `--rpc-rate-limit`.

> **Note on `--hop-data-dir`.** The path is used as-is with the node user's
> filesystem permissions — point it at a dedicated directory, not a shared or
> privileged path.

## RPC methods

### `hop_submit(data, recipients, signature, signer, submit_timestamp) -> SubmitResult`

Store a blob for the given list of recipients.

- `data`: raw bytes, must be ≤ `HopRuntimeApi::max_promotion_size()` (the runtime
  cap is authoritative — no separate node-side ceiling).
- `recipients`: up to **256** SCALE-encoded `MultiSigner` values (ed25519,
  sr25519, or ecdsa ephemeral public keys).
- `signature`: SCALE-encoded `MultiSignature` over
  `blake2_256(HOP_SUBMIT_CONTEXT || blake2_256(data) || submit_timestamp.to_le_bytes())`.
- `signer`: SCALE-encoded `MultiSigner` of the submitting account.
- `submit_timestamp`: wall-clock submit time in milliseconds since the Unix
  epoch. Bound into the signed payload; the runtime rejects promotions whose
  timestamp drifts too far from on-chain time, so the same `(data, signer,
  signature)` cannot be replayed indefinitely.

Submit fails with:
- `DataTooLarge` if `data.len() > HopRuntimeApi::max_promotion_size()`.
- `NotAuthorized` if `HopRuntimeApi::can_account_promote(account_id, data_len)`
  returns `false` (where `account_id` is `signer.into_account()`). The runtime
  sees `data_len` so it can express size-tiered authorization policies on top
  of the absolute cap.
- `RateLimited` if the per-account submit-rate or bandwidth bucket is empty.

Size and authorization are both checked *before* signature verification so
oversized or unauthorized floods don't force crypto work.

### `hop_claim(hash, signature) -> Bytes`

Read-only download of the blob. Requires a SCALE-encoded `MultiSignature`
from one of the recipients' ephemeral keypairs over
`blake2_256(HOP_CLAIM_CONTEXT || hash)`. Does **not** mark the recipient as
claimed — call `hop_ack` separately.

The blob may be deleted concurrently by another recipient's final ack;
callers must be prepared for `NotFound` between successive calls.

### `hop_ack(hash, signature) -> ()`

Mark the calling recipient as claimed. When all recipients have acked, the
entry is deleted. Idempotent: calling twice with the same signature is a
no-op, but if the entry has already been deleted (all recipients ack'd, or
it expired), the call returns `NotFound` — treat `NotFound` as a benign
terminal state.

Signature payload is `blake2_256(HOP_ACK_CONTEXT || hash)`.

### `hop_poolStatus() -> PoolStatus`

Returns `{ entryCount, totalBytes, maxBytes }` (camelCase on the wire).

## Error codes

| Code | Variant | Meaning |
|---|---|---|
| 1001 | `DataTooLarge` | Blob exceeds runtime-reported `max_promotion_size` |
| 1002 | `PoolFull` | Total pool capacity exhausted |
| 1003 | `DuplicateEntry` | A blob with this hash is already in the pool |
| 1004 | `NotFound` | No entry for this hash (expired, never submitted, or deleted after final ack) |
| 1005 | `EmptyData` | Blob is zero bytes |
| 1007 | `InvalidSignature` | Signature verification failed |
| 1008 | `NotRecipient` | No recipient's public key matches the claim/ack signature |
| 1009 | `NoRecipients` | Submit provided an empty recipient list |
| 1010 | `InvalidRecipientKey` | A recipient entry did not decode as `MultiSigner` |
| 1011 | `UserQuotaExceeded` | Sender's per-user quota (`--hop-max-user-size`) is full |
| 1012 | `NotAuthorized` | `HopRuntimeApi::can_account_promote` returned `false` for the signer |
| 1013 | `IoError` | Disk I/O failure |
| 1014 | `InvalidSigner` | Submit `signer` did not decode as `MultiSigner` |
| 1015 | `AlreadyClaimed` | Recipient has already ack'd and the entry was deleted |
| 1016 | `InvalidHashLength` | Hash input was not exactly 32 bytes |
| 1017 | `RuntimeApiError` | Runtime API call failed (authorization check, extrinsic construction, etc.) |
| 1018 | `TooManyRecipients` | Submit exceeded the 256-recipient cap |
| 1019 | `DuplicateRecipient` | Recipient list contains duplicates |
| 1020 | `RateLimited` | Per-account rate limit exceeded; response includes `retry_after_secs` |
| 1021 | `MissingDataDir` | Neither `--hop-data-dir` nor a chain database path was available |

## Metrics

| Metric | Type | Labels |
|---|---|---|
| `substrate_hop_pool_entries` / `_pool_bytes` / `_pool_max_bytes` | gauge | |
| `substrate_hop_pool_inserted_bytes_total` | counter | |
| `substrate_hop_pool_removed_total` | counter | `reason` (values below) |
| `substrate_hop_rpc_errors_total` | counter | `method` (wire name, e.g. `hop_submit`), `reason` (`HopError` variant) |
| `substrate_hop_promotions_confirmed_total` | counter | |
| `substrate_hop_promotion_backlog` | gauge | |
| `substrate_hop_maintenance_ticks_total` | counter | |

`reason` values, all pre-created at registration: `acked`, `expired_promoted`,
`expired_unpromoted` (an upper bound on loss; re-checking stops at
`MAX_PROMOTION_ATTEMPTS`), `corrupt`, `startup_dropped`.

## Limits and fixed parameters

- Max blob size: whatever `HopRuntimeApi::max_promotion_size()` returns on the
  current runtime — authoritative, no separate node-side ceiling.
- `MAX_RECIPIENTS` = 256 per entry (enforced by `BoundedVec` — corrupt on-disk
  `.meta` files with too many recipients fail to SCALE-decode and are
  discarded during startup recovery).
- Hash: Blake2-256.
- On-disk layout: 256 shard directories under `<data_dir>/blobs/` and
  `<data_dir>/meta/`.

## Graceful degradation

If the runtime doesn't implement `sp_hop::HopRuntimeApi`, `try_build_promoter`
logs a warning and the maintenance task runs in cleanup-only mode (no
promotion). This lets operators deploy an HOP-enabled node ahead of the
runtime upgrade that adds `HopRuntimeApi`.

## License

GPL-3.0-or-later WITH Classpath-exception-2.0

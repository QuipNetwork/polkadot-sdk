# Storage Chain Tip-Sync Fixtures

This test uses a generated parachain database where old transaction-storage
entries are present in chain state but absent from the local transaction column.
The tip-sync test starts from that snapshot, warp-syncs a fresh node, renews the
old entries, and verifies the bytes are fetched through bitswap.

## Bundle layout

The generator emits a single `tip-sync-100-bundle.tar.gz` (a zombienet-sdk
`BundleBuilder` artefact) containing:

| Member | Contents |
|---|---|
| `parachain-db.tgz` | `pruned-node` collator DB (`data/` + embedded `relay-data/`) |
| `relaychain-db.tgz` | `alice` relay validator DB (`data/`) |
| `manifest.json` | SDK schema + `user_data` carrying `SnapshotMetadata` plus both raw chain specs |

The consumer (`parachain_tip_sync_with_renewals`) downloads the bundle (or
accepts a local path via `STORAGE_CHAIN_BUNDLE`), unpacks it with
`zombienet_sdk::snapshot::untar_bundle`, and reads everything it needs from
the bundle — no other fixture files are required.

## Regenerating Fixtures

```bash
cd cumulus/zombienet/zombienet-sdk/tests/zombie_ci/storage_chain
./generate-snapshots.sh all
```

Useful phases:

```bash
./generate-snapshots.sh build
./generate-snapshots.sh snapshots-run        # writes tip-sync-100-bundle.tar.gz
./generate-snapshots.sh snapshots-test-local # runs the test against the local bundle
```

After local validation, upload `tip-sync-100-bundle.tar.gz` to the GCS path
configured in `fixture.rs` (`DEFAULT_BUNDLE_URL`). Do not commit the bundle.

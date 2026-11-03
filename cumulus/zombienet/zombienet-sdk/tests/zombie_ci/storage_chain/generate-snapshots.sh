#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_DIR="$(dirname "$(cargo locate-project --workspace --message-format plain)")/target/release"
SNAPSHOT_DIR="${ZOMBIENET_SDK_BASE_DIR:-/tmp/zombienet-storage-chain}"
BUNDLE_OUTPUT_DIR="${BUNDLE_OUTPUT_DIR:-$SCRIPT_DIR/fixtures/test-databases}"
BUNDLE_PATH="${BUNDLE_OUTPUT_DIR}/tip-sync-100-bundle.tar.gz"

usage() {
	cat <<EOF
Usage: $0 <phase>

Phases:
  build                  Build required binaries
  snapshots-run          Run the Rust snapshot generator (writes bundle.tar.gz)
  snapshots-test-local   Run the tip-sync test against the local bundle
  all                    Run all phases

Environment:
  ZOMBIENET_SDK_BASE_DIR  Zombienet base dir (default: /tmp/zombienet-storage-chain)
  BUNDLE_OUTPUT_DIR       Bundle output dir (default: ./fixtures/test-databases)
EOF
	exit 1
}

build_binaries() {
	cargo build --release -p polkadot --bin polkadot
	cargo build --release -p polkadot-parachain-bin --bin polkadot-parachain
}

snapshots_run() {
	mkdir -p "$SNAPSHOT_DIR" "$BUNDLE_OUTPUT_DIR"
	export PATH="$TARGET_DIR:$PATH"
	export RUST_LOG=info,zombienet_orchestrator=debug
	export ZOMBIE_PROVIDER=native
	export ZOMBIENET_SDK_BASE_DIR="$SNAPSHOT_DIR"
	export BUNDLE_OUTPUT_DIR

	cargo test --release \
		-p cumulus-zombienet-sdk-tests \
		--features zombie-ci,generate-snapshots \
		-- storage_chain::parachain_generate_db::parachain_generate_databases
}

snapshots_test_local() {
	[[ -f "$BUNDLE_PATH" ]] || { echo "missing bundle at $BUNDLE_PATH" >&2; exit 1; }

	export PATH="$TARGET_DIR:$PATH"
	export RUST_LOG=info,zombienet_orchestrator=debug
	export ZOMBIE_PROVIDER=native
	export STORAGE_CHAIN_BUNDLE="$BUNDLE_PATH"

	cargo test --release \
		-p cumulus-zombienet-sdk-tests \
		--features zombie-ci \
		-- storage_chain::parachain_tip_sync_with_renewals::parachain_tip_sync_with_renewals_test
}

all() {
	build_binaries
	snapshots_run
	snapshots_test_local
}

[[ $# -eq 0 ]] && usage

case "$1" in
	build) build_binaries ;;
	snapshots-run) snapshots_run ;;
	snapshots-test-local) snapshots_test_local ;;
	all) all ;;
	*) echo "Unknown phase: $1" >&2; usage ;;
esac

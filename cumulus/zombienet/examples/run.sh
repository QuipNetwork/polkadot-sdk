#!/usr/bin/env bash
set -e

if [ -z "$1" ]; then
    echo "Usage: $0 <network-file.toml>"
    echo "Available networks:"
    ls -1 "$(dirname "$0")"/*.toml
    exit 1
fi

NETWORK_FILE="$1"
SCRIPT_DIR="$(dirname "$0")"

# Resolve to absolute path if relative
if [[ ! "$NETWORK_FILE" = /* ]]; then
    if [ -f "$SCRIPT_DIR/$NETWORK_FILE" ]; then
        NETWORK_FILE="$SCRIPT_DIR/$NETWORK_FILE"
    fi
fi

if [ ! -f "$NETWORK_FILE" ]; then
    echo "Error: Network file '$NETWORK_FILE' not found"
    exit 1
fi

cargo build --release -p polkadot --bin polkadot-prepare-worker --bin polkadot-execute-worker --bin polkadot -p polkadot-parachain-bin --bin polkadot-parachain

RELEASE_DIR=$(dirname "$(cargo locate-project --workspace --message-format plain)")/target/release

export PATH=$RELEASE_DIR:$PATH

zombie-cli spawn --provider native "$NETWORK_FILE"

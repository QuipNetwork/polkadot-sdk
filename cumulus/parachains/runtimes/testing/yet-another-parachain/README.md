# Yet Another Parachain (YAP) Runtime

A parachain runtime used for testing purposes.

## Features

- `fast-runtime` - Faster block times for testing
- `runtime-benchmarks` - Enable runtime benchmarking

## Running with `polkadot-omni-node` locally and 500ms blocks

### Build

```bash
# From the polkadot-sdk root directory

# 1. Build the YAP runtime
cargo build --release -p yet-another-parachain-runtime

# 2. Build polkadot-omni-node
cargo build --release -p polkadot-omni-node
```

### Generate Chain Spec

Available presets: `development`, `local_testnet`

```bash
# Generate chain spec using the development preset
./target/release/polkadot-omni-node chain-spec-builder \
  --chain-spec-path yap-dev-spec.json \
  create \
  --chain-name "YAP Development" \
  --chain-id yap-dev \
  --relay-chain "rococo-local" \
  -t development \
  --runtime ./target/release/wbuild/yet-another-parachain-runtime/yet_another_parachain_runtime.wasm \
  named-preset development

```

### Run the Node

```bash
./target/release/polkadot-omni-node \
  --chain yap-dev-spec.json \
  --dev --dev-block-time 500\
  --tmp
```

## Connecting to the Node

Once running, you can connect to the node using:

- **Polkadot.js Apps**: https://polkadot.js.org/apps/?rpc=ws://127.0.0.1:9944
- **RPC endpoint**: `ws://127.0.0.1:9944`

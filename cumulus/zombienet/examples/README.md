# Zombienet Examples

## Prerequisites

Install the zombienet CLI:

```bash
cargo install zombie-cli
```

## Usage

```bash
./run.sh <network-file.toml>
```

The script will:
1. Build `polkadot`, `polkadot-prepare-worker`, `polkadot-execute-worker`, and `polkadot-parachain` in release mode
2. Add the release directory to `PATH`
3. Spawn the network using `zombie-cli`

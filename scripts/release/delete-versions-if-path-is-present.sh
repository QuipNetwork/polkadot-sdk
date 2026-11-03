#!/bin/bash

ROOT_TOML="./Cargo.toml"

echo "Processing $ROOT_TOML..."

# Find lines that have path = "..." and version = "..."
# and remove only the version = "..." part, regardless of other fields
sed -i.bak -E 's/(path\s*=\s*"[^"]*"\s*(,\s*[^,]*?)*)\s*,\s*version\s*=\s*"[^"]*"/\1/g' "$ROOT_TOML"

# Clean up backup
rm -f "${ROOT_TOML}.bak"

echo "Done. Removed version fields from local path dependencies."

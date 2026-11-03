#!/bin/bash

# Find all Cargo.toml files excluding the root,umbrella/Cargo.toml,
# substrate/frame/contracts/fixtures/build/Cargo.toml,
# substrate/frame/contracts/fixtures/contracts/common/Cargo.toml
find . -name "Cargo.toml" \
  ! -path "./Cargo.toml" \
  ! -path "./umbrella/Cargo.toml" \
  ! -path "./substrate/frame/contracts/fixtures/build/Cargo.toml" \
  ! -path "./substrate/frame/contracts/fixtures/contracts/common/Cargo.toml"| while read -r file; do

  echo "Processing $file..."

  # Find and replace path dependencies with "workspace = true"
  # Also ensure "workspace = true" comes before "default-features"
  awk '
    BEGIN { in_section = 0 }
    /^\[.*dependencies\]/   { in_section = 1; print; next }
    /^\[.*\]/               { in_section = 0; print; next }

    {
      if (in_section == 1 || in_section == 2) {
        if ($0 ~ /path *= *".*"/) {
          gsub(/path *= *".*"/, "workspace = true")
          # If default-features appears before workspace, reorder them
          if (match($0, /default-features *= *[a-z]+, *workspace *= *true/)) {
            # Extract the default-features part
            match($0, /default-features *= *[a-z]+/)
            df = substr($0, RSTART, RLENGTH)
            # Replace "df, workspace = true" with "workspace = true, df"
            gsub(/default-features *= *[a-z]+, *workspace *= *true/, "workspace = true, " df)
          }
        }
      }
      print
    }
  ' "$file" > "${file}.tmp" && mv "${file}.tmp" "$file"

done

echo "All applicable Cargo.toml files updated."

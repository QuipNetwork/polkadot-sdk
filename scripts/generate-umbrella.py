"""

Creates the Polkadot-SDK umbrella crate that re-exports all other crates.

This re-creates the `umbrella/` folder. Ensure that it does not contain any changes you want to keep.

Usage:
    python3 generate-umbrella.py --sdk <path> --version <version>

Example:
	python3 generate-umbrella.py --sdk .. --version 1.11.0
"""

import argparse
import os
import re
import toml
import shutil

from cargo_workspace import Workspace

"""
Crate names that should be excluded from the umbrella crate.
"""
def exclude(crate):
	name = crate.name
	if crate.metadata.get("polkadot-sdk.exclude-from-umbrella", False):
		return True

	# No fuzzers or examples:
	if "example" in name or name.endswith("fuzzer"):
		return True

	# No runtime crates:
	if name.endswith("-runtime"):
		# Note: this is a bit hacky. We should use custom crate metadata instead.
		return name != "sp-runtime" and name != "bp-runtime" and name != "frame-try-runtime"

	# Exclude snowbridge crates.
	if name.startswith("snowbridge-"):
		return True

	return False

def main(path, version):
	delete_umbrella(path)
	workspace = Workspace.from_path(path)
	print(f'Indexed {workspace}')

	std_crates = [] # name -> path. use list for sorting
	nostd_crates = []
	for crate in workspace.crates:
		if crate.name == 'polkadot-sdk':
			continue
		if not crate.publish:
			print(f"Skipping {crate.name} as it is not published")
			continue

		lib_path = os.path.dirname(crate.abs_path)
		manifest_path = os.path.join(lib_path, "Cargo.toml")
		lib_path = os.path.join(lib_path, "src", "lib.rs")
		path = os.path.dirname(crate.rel_path)

		# Guess which crates support no_std. Proc-macro crates are always no_std:
		with open(manifest_path, "r") as f:
			manifest = toml.load(f)
			if 'lib' in manifest and 'proc-macro' in manifest['lib']:
				if manifest['lib']['proc-macro']:
					nostd_crates.append((crate, path))
					continue

		# Crates without a lib.rs cannot be no_std
		if not os.path.exists(lib_path):
			print(f"Skipping {crate.name} as it does not have a 'src/lib.rs'")
			continue
		if exclude(crate):
			print(f"Skipping {crate.name} as it is in the exclude list")
			continue

		# No search for a no_std attribute:
		with open(lib_path, "r") as f:
			nostd_crate = False
			for line in f:
				line = line.strip()
				if line == "#![no_std]" or line == '#![cfg_attr(not(feature = "std"), no_std)]' or line == "#![cfg_attr(substrate_runtime, no_std)]":
					nostd_crate = True
					break
				elif "no_std" in line:
					print(line)

			if nostd_crate:
				nostd_crates.append((crate, path))
			else:
				std_crates.append((crate, path))

	# Sort by name
	std_crates.sort(key=lambda x: x[0].name)
	nostd_crates.sort(key=lambda x: x[0].name)

	runtime_crates = [crate for crate in nostd_crates if 'frame' in crate[0].name or crate[0].name.startswith('sp-')]
	all_crates = std_crates + nostd_crates
	all_crates.sort(key=lambda x: x[0].name)
	dependencies = {}

	for (crate, path) in nostd_crates:
		dependencies[crate.name] = {"path": f"../{path}", "default-features": False, "optional": True}

	for (crate, path) in std_crates:
		dependencies[crate.name] = {"path": f"../{path}", "default-features": False, "optional": True}

	# The empty features are filled by Zepter
	features = {
		"default": [ "std" ],
		"std": [],
		"runtime-benchmarks": [],
		"try-runtime": [],
		"serde": [],
		"experimental": [],
		"with-tracing": [],
		"runtime-full": list([f"{d.name}" for d, _ in nostd_crates]),
		"runtime": list([f"{d.name}" for d, _ in runtime_crates]),
		"node": ["std"] + list([f"{d.name}" for d, _ in std_crates]),
		"tuples-96": [],
	}

	manifest = {
		"package": {
			"name": "polkadot-sdk",
			"version": version,
			"edition": { "workspace": True },
			"authors": { "workspace": True },
			"description": "Polkadot SDK umbrella crate.",
			"homepage": { "workspace": True },
			"repository": { "workspace": True },
			"license": "Apache-2.0",
			"metadata": { "docs": { "rs": {
				"features": ["runtime-full", "node"],
				"targets": ["x86_64-unknown-linux-gnu"]
			}}}
		},
		"dependencies": dependencies,
		"features": features,
	}

	umbrella_dir = os.path.join(workspace.path, "umbrella")
	manifest_path = os.path.join(umbrella_dir, "Cargo.toml")
	lib_path = os.path.join(umbrella_dir, "src", "lib.rs")
	# create all dir
	os.makedirs(os.path.dirname(lib_path), exist_ok=True)
	# Write the manifest
	with open(manifest_path, "w") as f:
		toml_manifest = toml.dumps(manifest)
		f.write(toml_manifest)
		print(f"Wrote {manifest_path}")
	# and the lib.rs
	with open(lib_path, "w") as f:
		f.write('''// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(feature = "std"), no_std)]

//! Polkadot SDK umbrella crate re-exporting all other published crates.
//!
//! This helps to set a single version number for all your dependencies. Docs are in the
//! `polkadot-sdk-docs` crate.

// This file is auto-generated and checked by the CI.  You can edit it manually, but it must be
// exactly the way that the CI expects it.
''')

		for crate, _ in all_crates:
			use = crate.name.replace("-", "_")
			desc = crate.description if crate.description.endswith(".") else crate.description + "."
			f.write(f'\n/// {desc}')
			f.write(f'\n#[cfg(feature = "{crate.name}")]\n')
			f.write(f"pub use {use};\n")

		print(f"Wrote {lib_path}")

	add_to_workspace(workspace.path)

"""
Delete the umbrella folder and remove the umbrella crate from the workspace.
"""
def delete_umbrella(path):
	# remove the umbrella crate from the workspace
	manifest = os.path.join(path, "Cargo.toml")
	manifest = open(manifest, "r").read()
	manifest = re.sub(r'\s+"umbrella",\n', "", manifest)
	with open(os.path.join(path, "Cargo.toml"), "w") as f:
		f.write(manifest)
	umbrella_dir = os.path.join(path, "umbrella")
	if os.path.exists(umbrella_dir):
		print(f"Deleting {umbrella_dir}")
		os.remove(os.path.join(umbrella_dir, "Cargo.toml"))
		shutil.rmtree(os.path.join(umbrella_dir, "src"))

"""
Create the umbrella crate and add it to the workspace.
"""
def add_to_workspace(path):
	manifest = os.path.join(path, "Cargo.toml")
	manifest = open(manifest, "r").read()
	manifest = re.sub(r'^members = \[', 'members = [\n        "umbrella",', manifest, flags=re.M)
	with open(os.path.join(path, "Cargo.toml"), "w") as f:
		f.write(manifest)

	os.chdir(path) # hack
	os.system("cargo metadata --format-version 1 > /dev/null") # update the lockfile
	os.system(f"zepter") # enable the features
	os.system(f"taplo format --config .config/taplo.toml Cargo.toml umbrella/Cargo.toml")

def parse_args():
	parser = argparse.ArgumentParser(description="Create a polkadot-sdk crate")
	parser.add_argument("--sdk", type=str, default="polkadot-sdk", help="Path to the polkadot-sdk crate")
	parser.add_argument("--version", type=str, help="Version of the polkadot-sdk crate")
	return parser.parse_args()

if __name__ == "__main__":
	args = parse_args()
	main(args.sdk, args.version)

// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Shared code for building fixtures from Rust source.
//! Used by both build.rs and test code.

use anyhow::{bail, Context, Result};
use cargo_metadata::MetadataCommand;
use pallet_revive_uapi::precompiles::INTERFACE_DIR;
use std::{
	env, fs,
	io::Write,
	path::{Path, PathBuf},
	process::Command,
};

pub const OVERRIDE_RUSTUP_TOOLCHAIN_ENV_VAR: &str = "PALLET_REVIVE_FIXTURES_RUSTUP_TOOLCHAIN";
pub const OVERRIDE_STRIP_ENV_VAR: &str = "PALLET_REVIVE_FIXTURES_STRIP";
pub const OVERRIDE_OPTIMIZE_ENV_VAR: &str = "PALLET_REVIVE_FIXTURES_OPTIMIZE";
/// Do not build the fixtures, they will resolve to `None`.
///
/// Depending on the usage, they will probably panic at runtime.
pub const SKIP_PALLET_REVIVE_FIXTURES: &str = "SKIP_PALLET_REVIVE_FIXTURES";

/// A contract entry.
#[derive(Clone)]
pub struct Entry {
	/// The path to the contract source file.
	pub path: PathBuf,
	/// The type of the contract (rust or solidity).
	pub contract_type: ContractType,
}

#[derive(Clone, Copy)]
pub enum ContractType {
	Rust,
	Solidity,
}

/// Type of EVM bytecode to extract from Solidity compiler output.
#[derive(Clone, Copy)]
enum EvmByteCodeType {
	InitCode,
	RuntimeCode,
}

impl EvmByteCodeType {
	fn json_key(&self) -> &'static str {
		match self {
			Self::InitCode => "bytecode",
			Self::RuntimeCode => "deployedBytecode",
		}
	}
}

impl Entry {
	/// Create a new contract entry from the given path.
	pub fn new(path: PathBuf, contract_type: ContractType) -> Self {
		Self { path, contract_type }
	}

	/// Return the path to the contract source file.
	pub fn path(&self) -> &str {
		self.path.to_str().expect("path is valid unicode; qed")
	}

	/// Return the name of the contract.
	pub fn name(&self) -> &str {
		self.path
			.file_stem()
			.expect("file exits; qed")
			.to_str()
			.expect("name is valid unicode; qed")
	}

	/// Return the name of the bytecode file.
	pub fn out_filename(&self) -> String {
		match self.contract_type {
			ContractType::Rust => format!("{}.polkavm", self.name()),
			ContractType::Solidity => format!("{}.resolc.polkavm", self.name()),
		}
	}
}

/// Collect all contract entries from the given source directory.
pub fn collect_entries(contracts_dir: &Path) -> Vec<Entry> {
	fs::read_dir(contracts_dir)
		.expect("src dir exists; qed")
		.filter_map(|file| {
			let path = file.expect("file exists; qed").path();
			let extension = path.extension();

			match extension.and_then(|ext| ext.to_str()) {
				Some("rs") => Some(Entry::new(path, ContractType::Rust)),
				Some("sol") => Some(Entry::new(path, ContractType::Solidity)),
				_ => None,
			}
		})
		.collect::<Vec<_>>()
}

/// Create a `Cargo.toml` to compile the given Rust contract entries.
/// If fixtures_dir is provided, uses cargo metadata to resolve the uapi dependency.
/// Otherwise, uses a hardcoded path relative to CARGO_MANIFEST_DIR.
pub fn create_cargo_toml<'a>(
	fixtures_dir: Option<&Path>,
	entries: impl Iterator<Item = &'a Entry>,
	output_dir: &Path,
) -> Result<()> {
	let mut cargo_toml: toml::Value = toml::from_str(include_str!("../build/_Cargo.toml"))?;
	let uapi_dep = cargo_toml["dependencies"]["uapi"].as_table_mut().unwrap();

	// Set uapi dependency path
	if let Some(fixtures_dir) = fixtures_dir {
		// Use cargo metadata to resolve the uapi dependency
		let manifest_path = fixtures_dir.join("Cargo.toml");
		let metadata = MetadataCommand::new().manifest_path(&manifest_path).exec().unwrap();
		let dependency_graph = metadata.resolve.unwrap();

		// Resolve the pallet-revive-fixtures package id
		let fixtures_pkg_id = metadata
			.packages
			.iter()
			.find(|pkg| pkg.manifest_path.as_std_path() == manifest_path)
			.map(|pkg| pkg.id.clone())
			.unwrap();
		let fixtures_pkg_node =
			dependency_graph.nodes.iter().find(|node| node.id == fixtures_pkg_id).unwrap();

		// Get the pallet-revive-uapi package id
		let uapi_pkg_id = fixtures_pkg_node
			.deps
			.iter()
			.find(|dep| dep.name == "pallet_revive_uapi")
			.map(|dep| dep.pkg.clone())
			.expect("pallet-revive-uapi is a build dependency of pallet-revive-fixtures; qed");

		// Get pallet-revive-uapi package
		let uapi_pkg = metadata.packages.iter().find(|pkg| pkg.id == uapi_pkg_id).unwrap();

		if uapi_pkg.source.is_none() {
			uapi_dep.insert(
				"path".to_string(),
				toml::Value::String(
					fixtures_dir.join("../uapi").canonicalize()?.to_str().unwrap().to_string(),
				),
			);
		} else {
			uapi_dep
				.insert("version".to_string(), toml::Value::String(uapi_pkg.version.to_string()));
		}
	} else {
		// Use simple hardcoded path
		let manifest_dir = env!("CARGO_MANIFEST_DIR");
		let uapi_path = PathBuf::from(manifest_dir).parent().unwrap().join("uapi");
		uapi_dep.insert(
			"path".to_string(),
			toml::Value::String(uapi_path.to_str().unwrap().to_string()),
		);
	}

	// Set binary targets
	cargo_toml["bin"] = toml::Value::Array(
		entries
			.map(|entry| {
				let name = entry.name();
				let path = entry.path();
				toml::Value::Table(toml::toml! {
					name = name
					path = path
				})
			})
			.collect::<Vec<_>>(),
	);

	let cargo_toml = toml::to_string_pretty(&cargo_toml)?;
	fs::write(output_dir.join("Cargo.toml"), cargo_toml.clone())
		.with_context(|| format!("Failed to write {cargo_toml:?}"))?;
	Ok(())
}

/// Invoke cargo build to compile contracts to RISC-V ELF.
pub fn invoke_build(current_dir: &Path) -> Result<()> {
	let toolchain =
		env::var(OVERRIDE_RUSTUP_TOOLCHAIN_ENV_VAR).or_else(|_| env::var("RUSTUP_TOOLCHAIN"));

	// Detect rustc major.minor for the version-gated flags below.
	let (major, minor) = {
		let mut cmd = Command::new("rustc");
		if let Ok(toolchain) = &toolchain {
			cmd.env("RUSTUP_TOOLCHAIN", toolchain);
		}
		let out = cmd.arg("--version").output().context("rustc --version failed")?;
		let ver = String::from_utf8(out.stdout).context("utf8 from rustc --version failed")?;
		let ver_num = ver
			.split_whitespace()
			.nth(1)
			.ok_or_else(|| anyhow::anyhow!("unexpected rustc --version output: {ver}"))?;
		let mut parts = ver_num.split('.');
		let major: u32 = parts
			.next()
			.ok_or_else(|| anyhow::anyhow!("missing major version"))?
			.parse()
			.context("invalid major version")?;
		let minor: u32 = parts
			.next()
			.ok_or_else(|| anyhow::anyhow!("missing minor version"))?
			.parse()
			.context("invalid minor version")?;
		(major, minor)
	};

	// 1.92+: `-Cpanic=immediate-abort` is a stable rustflag.
	let new_immediate_abort = major > 1 || (major == 1 && minor >= 92);
	// 1.95+: `-Z json-target-spec` was added; later rustc requires it for any
	// `--target=*.json` invocation. Pre-1.95 doesn't know the flag.
	let needs_json_target_spec = major > 1 || (major == 1 && minor >= 95);

	// on newer version this is a stable compiler flag
	let encoded_rustflags = if new_immediate_abort {
		["-Dwarnings", "-Zunstable-options", "-Cpanic=immediate-abort"].join("\x1f")
	} else {
		["-Dwarnings"].join("\x1f")
	};

	let mut args = polkavm_linker::TargetJsonArgs::default();
	args.is_64_bit = true;

	let mut build_command = Command::new("cargo");
	build_command
		.current_dir(current_dir)
		.env_clear()
		.env("PATH", env::var("PATH").unwrap_or_default())
		.env("CARGO_ENCODED_RUSTFLAGS", encoded_rustflags)
		.env("RUSTUP_HOME", env::var("RUSTUP_HOME").unwrap_or_default())
		.env("RUSTC_BOOTSTRAP", "1")
		.args(["build", "--release", "-Zbuild-std=core"])
		.arg("--target")
		.arg(polkavm_linker::target_json_path(args).unwrap());

	// 1.95+: opt into JSON target specs explicitly (required by recent rustc).
	if needs_json_target_spec {
		build_command.arg("-Zjson-target-spec");
	}

	// on older versions this is a unstable cargo cli argument
	if !new_immediate_abort {
		build_command.arg("-Zbuild-std-features=panic_immediate_abort");
	}

	if let Ok(toolchain) = &toolchain {
		build_command.env("RUSTUP_TOOLCHAIN", &toolchain);
	}

	let build_res = build_command.output().expect("failed to execute process");

	if !build_res.status.success() {
		let stderr = String::from_utf8_lossy(&build_res.stderr);
		eprintln!("{}", stderr);
		anyhow::bail!("Failed to build contracts");
	}

	Ok(())
}

#[allow(dead_code)]
/// Compile a Rust contract source to RISC-V ELF.
fn compile_rust_to_elf(
	contract_path: &Path,
	contract_name: &str,
	output_dir: &Path,
) -> Result<PathBuf> {
	// Create Cargo.toml with single entry
	let entry = Entry::new(contract_path.to_path_buf(), ContractType::Rust);
	create_cargo_toml(None, std::iter::once(&entry), output_dir)?;

	// Build
	invoke_build(output_dir)?;

	// Return path to ELF
	let elf_path = output_dir
		.join("target/riscv64emac-unknown-none-polkavm/release")
		.join(contract_name);

	if !elf_path.exists() {
		anyhow::bail!("ELF not found at {:?}", elf_path);
	}

	Ok(elf_path)
}

#[allow(dead_code)]
/// Link a RISC-V ELF to PolkaVM bytecode.
fn link_elf_to_polkavm(elf_path: &Path) -> Result<Vec<u8>> {
	let elf_bytes = std::fs::read(elf_path)
		.with_context(|| format!("Failed to read ELF from {:?}", elf_path))?;

	let config = polkavm_linker::Config::default();
	let linked = polkavm_linker::program_from_elf(
		config,
		polkavm_linker::TargetInstructionSet::ReviveV1,
		&elf_bytes,
	)
	.map_err(|err| {
		anyhow::anyhow!("Failed to link polkavm program from {:?}: {}", elf_path, err)
	})?;

	Ok(linked)
}

// dead_code - it is used by test code only
#[allow(dead_code)]
/// Compile a Rust contract source all the way to PolkaVM bytecode.
pub fn compile_rust_to_polkavm(
	contract_path: &Path,
	contract_name: &str,
	temp_dir: &Path,
) -> Result<Vec<u8>> {
	let elf_path = compile_rust_to_elf(contract_path, contract_name, temp_dir)?;
	link_elf_to_polkavm(&elf_path)
}

/// Post-process the compiled code.
pub fn post_process(input_path: &Path, output_path: &Path) -> Result<()> {
	let strip = env::var(OVERRIDE_STRIP_ENV_VAR).map_or(false, |value| value == "1");
	let optimize = env::var(OVERRIDE_OPTIMIZE_ENV_VAR).map_or(true, |value| value == "1");

	let mut config = polkavm_linker::Config::default();
	config.set_strip(strip);
	config.set_optimize(optimize);
	let orig = fs::read(input_path).with_context(|| format!("Failed to read {input_path:?}"))?;
	let linked = polkavm_linker::program_from_elf(
		config,
		polkavm_linker::TargetInstructionSet::ReviveV1,
		orig.as_ref(),
	)
	.map_err(|err| anyhow::format_err!("Failed to link polkavm program: {}", err))?;
	fs::write(output_path, linked).with_context(|| format!("Failed to write {output_path:?}"))?;
	Ok(())
}

/// Compile a Solidity contract using standard JSON interface.
fn compile_with_standard_json(
	compiler: &str,
	contracts_dir: &Path,
	solidity_entries: &[&Entry],
) -> Result<serde_json::Value> {
	let mut remappings = vec![format!("@revive/={INTERFACE_DIR}")];
	let oz_root = contracts_dir.join("external/openzeppelin/");
	remappings.push(format!("@openzeppelin/={}", oz_root.display()));
	let mut input_json = serde_json::json!({
		"language": "Solidity",
		"sources": {},
		"settings": {
			"optimizer": {
				"enabled": false,
				"runs": 200
			},
			"remappings": remappings,
			"outputSelection": serde_json::json!({
				"*": {
					"*": ["evm.bytecode", "evm.deployedBytecode"]
				}
			}),
		}
	});

	// Add all Solidity files to the input
	for entry in solidity_entries {
		let source_code = fs::read_to_string(entry.path())
			.with_context(|| format!("Failed to read Solidity source: {}", entry.path()))?;

		let file_key = entry.path().split('/').last().unwrap_or(entry.name());
		input_json["sources"][file_key] = serde_json::json!({
			"content": source_code
		});
	}

	let compiler_output = Command::new(compiler)
		.current_dir(contracts_dir)
		.arg("--allow-paths")
		.arg(INTERFACE_DIR)
		.arg("--standard-json")
		.stdin(std::process::Stdio::piped())
		.stdout(std::process::Stdio::piped())
		.stderr(std::process::Stdio::piped())
		.spawn()
		.with_context(|| {
			format!(
				"Failed to execute {compiler}. Make sure {compiler} is installed or \
				set env variable `SKIP_PALLET_REVIVE_FIXTURES=1` to skip fixtures compilation."
			)
		})?;

	let mut stdin = compiler_output.stdin.as_ref().unwrap();
	stdin
		.write_all(input_json.to_string().as_bytes())
		.with_context(|| format!("Failed to write to {} stdin", compiler))?;
	let _ = stdin;

	let compiler_result = compiler_output
		.wait_with_output()
		.with_context(|| format!("Failed to wait for {} output", compiler))?;

	if !compiler_result.status.success() {
		let stderr = String::from_utf8_lossy(&compiler_result.stderr);
		bail!("{} compilation failed: {}", compiler, stderr);
	}

	// Parse JSON output
	let compiler_json: serde_json::Value = serde_json::from_slice(&compiler_result.stdout)
		.with_context(|| format!("Failed to parse {} JSON output", compiler))?;

	// Abort on errors
	if let Some(errors) = compiler_json.get("errors") {
		if errors
			.as_array()
			.unwrap()
			.iter()
			.any(|object| object.get("severity").unwrap().as_str().unwrap() == "error")
		{
			bail!(
				"failed to compile the Solidity fixtures: {}",
				serde_json::to_string_pretty(errors)?
			);
		}
	}

	Ok(compiler_json)
}

/// Extract bytecode from compiler JSON output and write binary files.
fn extract_and_write_bytecode(
	compiler_json: &serde_json::Value,
	out_dir: &Path,
	file_suffix: &str,
	bytecode_type: EvmByteCodeType,
) -> Result<()> {
	if let Some(contracts) = compiler_json["contracts"].as_object() {
		for (_file_key, file_contracts) in contracts {
			if let Some(contract_map) = file_contracts.as_object() {
				for (contract_name, contract_data) in contract_map {
					// Navigate through the JSON path to find the bytecode
					let mut current = contract_data;
					for path_segment in ["evm", bytecode_type.json_key(), "object"] {
						if let Some(next) = current.get(path_segment) {
							current = next;
						} else {
							// Skip if path doesn't exist (e.g., contract has no bytecode)
							continue;
						}
					}

					if let Some(bytecode_obj) = current.as_str() {
						let bytecode_hex = bytecode_obj.strip_prefix("0x").unwrap_or(bytecode_obj);
						let binary_content = hex::decode(bytecode_hex).map_err(|e| {
							anyhow::anyhow!("Failed to decode hex for {contract_name}: {e}")
						})?;

						let out_path = out_dir.join(format!("{}{}", contract_name, file_suffix));
						fs::write(&out_path, binary_content).with_context(|| {
							format!("Failed to write {out_path:?} for {contract_name}")
						})?;
					}
				}
			}
		}
	}
	Ok(())
}

/// Compile Solidity contracts using both solc and resolc.
pub fn compile_solidity_contracts(
	contracts_dir: &Path,
	out_dir: &Path,
	entries: &[Entry],
) -> Result<()> {
	let solidity_entries: Vec<_> = entries
		.iter()
		.filter(|entry| matches!(entry.contract_type, ContractType::Solidity))
		.collect();

	if solidity_entries.is_empty() {
		return Ok(());
	}

	let evm_only = vec!["HostEvmOnly"];
	let solidity_entries_pvm: Vec<_> = solidity_entries
		.iter()
		.cloned()
		.filter(|entry| !evm_only.contains(&entry.path.file_stem().unwrap().to_str().unwrap()))
		.collect();

	// Compile with solc for EVM bytecode
	let json = compile_with_standard_json("solc", contracts_dir, &solidity_entries)?;
	extract_and_write_bytecode(&json, out_dir, ".sol.bin", EvmByteCodeType::InitCode)?;
	extract_and_write_bytecode(&json, out_dir, ".sol.runtime.bin", EvmByteCodeType::RuntimeCode)?;

	// Compile with resolc for PVM bytecode
	let json = compile_with_standard_json("resolc", contracts_dir, &solidity_entries_pvm)?;
	extract_and_write_bytecode(&json, out_dir, ".resolc.polkavm", EvmByteCodeType::InitCode)?;

	Ok(())
}

/// Write the compiled Rust contracts to the given output directory.
pub fn write_output(build_dir: &Path, out_dir: &Path, entries: Vec<Entry>) -> Result<()> {
	for entry in entries {
		if matches!(entry.contract_type, ContractType::Rust) {
			post_process(
				&build_dir
					.join("target/riscv64emac-unknown-none-polkavm/release")
					.join(entry.name()),
				&out_dir.join(entry.out_filename()),
			)?;
		}
	}

	Ok(())
}

/// Generate the fixture_location.rs file with macros and sol! definitions.
pub fn generate_fixture_location(temp_dir: &Path, out_dir: &Path, entries: &[Entry]) -> Result<()> {
	let mut file = fs::File::create(temp_dir.join("fixture_location.rs"))
		.context("Failed to create fixture_location.rs")?;

	let (fixtures, fixtures_resolc) = if env::var("SKIP_PALLET_REVIVE_FIXTURES").is_err() {
		(
			format!(
				r#"Some(include_bytes!(concat!("{}", "/", $name, ".polkavm")))"#,
				out_dir.display()
			),
			format!(
				r#"Some(include_bytes!(concat!("{}", "/", $name, ".resolc.polkavm")))"#,
				out_dir.display()
			),
		)
	} else {
		("None".into(), "None".into())
	};

	write!(
		file,
		r#"
			#[allow(dead_code)]
			const FIXTURE_DIR: &str = "{0}";

			#[macro_export]
			macro_rules! fixture {{
				($name: literal) => {{
					{fixtures}
				}};
			}}

			#[macro_export]
			macro_rules! fixture_resolc {{
				($name: literal) => {{
					{fixtures_resolc}
				}};
			}}
		"#,
		out_dir.display()
	)
	.context("Failed to write to fixture_location.rs")?;

	// Generate sol! macros for Solidity contracts
	for entry in entries.iter().filter(|e| matches!(e.contract_type, ContractType::Solidity)) {
		let relative_path = format!("contracts/{}", entry.path().split('/').last().unwrap());
		writeln!(file, r#"#[cfg(feature = "std")] alloy_core::sol!("{}");"#, relative_path)
			.context("Failed to write sol! macro to fixture_location.rs")?;
	}

	Ok(())
}

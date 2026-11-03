// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! The AuRa consensus algorithm for parachains.
//!
//! This extends the Substrate provided AuRa consensus implementation to make it compatible for
//! parachains.
//!
//! For more information about AuRa, the Substrate crate should be checked.

use codec::Encode;
use cumulus_primitives_core::{relay_chain::HeadData, PersistedValidationData};
use polkadot_node_primitives::PoV;
use polkadot_primitives::{BlockNumber as RBlockNumber, Hash as RHash};
use sp_runtime::traits::{Block as BlockT, NumberFor};
use std::{fs, fs::File, path::PathBuf};

mod import_queue;

pub use import_queue::{build_verifier, import_queue, BuildVerifierParams, ImportQueueParams};
pub use sc_consensus_aura::{
	slot_duration, standalone::slot_duration_at, AuraVerifier, BuildAuraWorkerParams,
	SlotProportion,
};
pub use sc_consensus_slots::InherentDataProviderExt;

pub mod collator;
pub mod collators;
pub mod equivocation_import_queue;

const LOG_TARGET: &str = "aura::cumulus";

/// Export the given `pov` to the file system at `path`.
///
/// The file will be named `block_hash_block_number.pov`.
///
/// The `parent_header`, `relay_parent_storage_root` and `relay_parent_number` will also be
/// stored in the file alongside the `pov`. This enables stateless validation of the `pov`.
pub(crate) fn export_pov_to_path<Block: BlockT>(
	path: PathBuf,
	pov: PoV,
	block_hash: Block::Hash,
	block_number: NumberFor<Block>,
	parent_header: Block::Header,
	relay_parent_storage_root: RHash,
	relay_parent_number: RBlockNumber,
	max_pov_size: u32,
) {
	if let Err(error) = fs::create_dir_all(&path) {
		tracing::error!(target: LOG_TARGET, %error, path = %path.display(), "Failed to create PoV export directory");
		return;
	}

	let mut file = match File::create(path.join(format!("{block_hash:?}_{block_number}.pov"))) {
		Ok(f) => f,
		Err(error) => {
			tracing::error!(target: LOG_TARGET, %error, "Failed to export PoV.");
			return;
		},
	};

	pov.encode_to(&mut file);
	PersistedValidationData {
		parent_head: HeadData(parent_header.encode()),
		relay_parent_number,
		relay_parent_storage_root,
		max_pov_size,
	}
	.encode_to(&mut file);
}

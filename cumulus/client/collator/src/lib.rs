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

//! Cumulus Collator implementation for Substrate.
use polkadot_node_primitives::CollationGenerationConfig;
use polkadot_node_subsystem::messages::{CollationGenerationMessage, CollatorProtocolMessage};
use polkadot_overseer::Handle as OverseerHandle;
use polkadot_primitives::{CollatorPair, Id as ParaId};
pub mod service;

/// Relay-chain-driven collators are those whose block production is driven purely
/// by new relay chain blocks and the most recently included parachain blocks
/// within them.
///
/// This method of driving collators is not suited to anything but the most simple parachain
/// consensus mechanisms, and this module may soon be deprecated.
pub mod relay_chain_driven {
	use futures::{
		channel::{mpsc, oneshot},
		prelude::*,
	};
	use polkadot_node_primitives::{CollationGenerationConfig, CollationResult};
	use polkadot_node_subsystem::messages::{CollationGenerationMessage, CollatorProtocolMessage};
	use polkadot_overseer::Handle as OverseerHandle;
	use polkadot_primitives::{CollatorPair, Id as ParaId};

	use cumulus_primitives_core::{relay_chain::Hash as PHash, PersistedValidationData};

	/// A request to author a collation, based on the advancement of the relay chain.
	///
	/// See the module docs for more info on relay-chain-driven collators.
	pub struct CollationRequest {
		relay_parent: PHash,
		pvd: PersistedValidationData,
		sender: oneshot::Sender<Option<CollationResult>>,
	}

	impl CollationRequest {
		/// Get the relay parent of the collation request.
		pub fn relay_parent(&self) -> &PHash {
			&self.relay_parent
		}

		/// Get the [`PersistedValidationData`] for the request.
		pub fn persisted_validation_data(&self) -> &PersistedValidationData {
			&self.pvd
		}

		/// Complete the request with a collation, if any.
		pub fn complete(self, collation: Option<CollationResult>) {
			let _ = self.sender.send(collation);
		}
	}

	/// Initialize the collator with Polkadot's collation-generation
	/// subsystem, returning a stream of collation requests to handle.
	pub async fn init(
		key: CollatorPair,
		para_id: ParaId,
		overseer_handle: OverseerHandle,
	) -> mpsc::Receiver<CollationRequest> {
		let mut overseer_handle = overseer_handle;

		let (stream_tx, stream_rx) = mpsc::channel(0);
		let config = CollationGenerationConfig {
			key,
			para_id,
			collator: Some(Box::new(move |relay_parent, validation_data| {
				// Cloning the channel on each usage effectively makes the channel
				// unbounded. The channel is actually bounded by the block production
				// and consensus systems of Polkadot, which limits the amount of possible
				// blocks.
				let mut stream_tx = stream_tx.clone();
				let validation_data = validation_data.clone();
				Box::pin(async move {
					let (this_tx, this_rx) = oneshot::channel();
					let request =
						CollationRequest { relay_parent, pvd: validation_data, sender: this_tx };

					if stream_tx.send(request).await.is_err() {
						return None;
					}

					this_rx.await.ok().flatten()
				})
			})),
		};

		overseer_handle
			.send_msg(CollationGenerationMessage::Initialize(config), "StartCollator")
			.await;

		overseer_handle
			.send_msg(CollatorProtocolMessage::CollateOn(para_id), "StartCollator")
			.await;

		stream_rx
	}
}

/// Initialize the collation-related subsystems on the relay-chain side.
///
/// This must be done prior to collation, and does not set up any callback for collation.
/// For callback-driven collators, use the [`relay_chain_driven`] module.
pub async fn initialize_collator_subsystems(
	overseer_handle: &mut OverseerHandle,
	key: CollatorPair,
	para_id: ParaId,
	reinitialize: bool,
) {
	let config = CollationGenerationConfig { key, para_id, collator: None };

	if reinitialize {
		overseer_handle
			.send_msg(CollationGenerationMessage::Reinitialize(config), "StartCollator")
			.await;
	} else {
		overseer_handle
			.send_msg(CollationGenerationMessage::Initialize(config), "StartCollator")
			.await;
	}

	overseer_handle
		.send_msg(CollatorProtocolMessage::CollateOn(para_id), "StartCollator")
		.await;
}

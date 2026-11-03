// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Configuration of the statement protocol

use std::time;

/// Interval at which we propagate statements;
pub(crate) const PROPAGATE_TIMEOUT: time::Duration = time::Duration::from_millis(1000);

/// Maximum allowed size for a statement notification.
pub const MAX_STATEMENT_NOTIFICATION_SIZE: u64 = 1024 * 1024;

/// Minimum wire size of a valid statement: field-count prefix, `AuthenticityProof` and `Expiry`.
pub const MIN_ENCODED_STATEMENT_SIZE: usize = 108;

/// Most statements a sender can pack into a full [`MAX_STATEMENT_NOTIFICATION_SIZE`]
/// notification. A batch declaring more is rejected before decoding.
pub const MAX_STATEMENTS_PER_NOTIFICATION: usize = (MAX_STATEMENT_NOTIFICATION_SIZE as usize -
	crate::V1_ENVELOPE_OVERHEAD) /
	MIN_ENCODED_STATEMENT_SIZE;

/// Soft limit on encoded statement chunks held in flight across all peers, shared by initial-sync
/// and propagation sends. Since admission is checked before adding the next whole notification, it
/// may be exceeded by less than one maximum-sized notification.
pub const MAX_SEND_IN_FLIGHT_BYTES: u64 = 16 * MAX_STATEMENT_NOTIFICATION_SIZE;

/// Part of [`MAX_SEND_IN_FLIGHT_BYTES`] withheld from propagation while initial syncs are
/// pending. Freed budget is otherwise reclaimed synchronously by parked propagations, while
/// sync bursts only check on a timer and would always find the budget full.
pub const INITIAL_SYNC_RESERVED_BYTES: u64 = 4 * MAX_STATEMENT_NOTIFICATION_SIZE;

/// Maximum number of statement hashes queued for propagation to one peer.
/// On overflow the oldest hashes are dropped first.
pub const MAX_PROPAGATION_OUTBOX_LEN: usize = 64 * 1024;

/// Maximum number of statement validation request we keep at any moment.
pub const MAX_PENDING_STATEMENTS: usize = 2 * 1024 * 1024;

/// Default maximum statements per second before rate limiting kicks in.
pub const DEFAULT_STATEMENTS_PER_SECOND: u32 = 50_000;

/// Burst capacity coefficient for the rate limiter.
pub const STATEMENTS_BURST_COEFFICIENT: u32 = 5;

#[cfg(test)]
mod tests {
	use super::*;
	use codec::Encode;
	use sp_statement_store::{Proof, Statement};

	#[test]
	fn min_encoded_statement_size_matches_encoder() {
		// The smallest statement admission accepts: a proof and an expiry.
		let min = [
			Proof::Sr25519 { signature: [0u8; 64], signer: [0u8; 32] },
			Proof::Ed25519 { signature: [0u8; 64], signer: [0u8; 32] },
			Proof::Secp256k1Ecdsa { signature: [0u8; 65], signer: [0u8; 33] },
		]
		.into_iter()
		.map(|proof| {
			// Stops compiling when a `Proof` variant is added, so the list above stays complete.
			match proof {
				Proof::Sr25519 { .. } | Proof::Ed25519 { .. } | Proof::Secp256k1Ecdsa { .. } => (),
			}
			let mut statement = Statement::new();
			statement.set_proof(proof);
			statement.set_expiry(0);
			statement.encode().len()
		})
		.min()
		.expect("the proof list is nonempty");
		assert_eq!(min, MIN_ENCODED_STATEMENT_SIZE);
	}
}

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

//! Test utilities for the statement store

use sp_core::{sr25519, Encode, Pair};
use sp_statement_store::{statement_allowance_key, Channel, StatementAllowance, Topic};

/// Generate a deterministic keypair for a given client index
pub fn get_keypair(idx: u32) -> sr25519::Pair {
	sr25519::Pair::from_string(&format!("//StatementClient//{idx}"), None)
		.expect("Derivation path is always valid; qed")
}

/// Creates uniform allowance storage items for a range of participants
pub fn create_uniform_allowance_items(
	count: u32,
	allowance: StatementAllowance,
) -> Vec<(Vec<u8>, Vec<u8>)> {
	let allowance_encoded = allowance.encode();
	let mut items = Vec::with_capacity(count as usize);
	for idx in 0..count {
		let keypair = get_keypair(idx);
		let account_id = keypair.public();
		let storage_key = statement_allowance_key(account_id.0);
		items.push((storage_key.to_vec(), allowance_encoded.clone()));
	}
	items
}

/// Creates storage items for custom per-participant allowances
pub fn create_allowance_items(allowances: &[(u32, StatementAllowance)]) -> Vec<(Vec<u8>, Vec<u8>)> {
	let mut items = Vec::with_capacity(allowances.len());
	for (idx, allowance) in allowances {
		let keypair = get_keypair(*idx);
		let account_id = keypair.public();
		let storage_key = statement_allowance_key(account_id.0);
		items.push((storage_key.to_vec(), allowance.encode()));
	}
	items
}

/// Creates a signed statement with the given topics, channel, data, and expiry
pub fn create_test_statement(
	keypair: &sr25519::Pair,
	topics: &[Topic],
	channel: Option<Channel>,
	data: Vec<u8>,
	expiry_ts: u32,
	seq: u32,
) -> sp_statement_store::Statement {
	let mut statement = sp_statement_store::Statement::new();
	for (i, topic) in topics.iter().enumerate() {
		statement.set_topic(i, *topic);
	}
	if let Some(ch) = channel {
		statement.set_channel(ch);
	}
	statement.set_plain_data(data);
	statement.set_expiry_from_parts(expiry_ts, seq);
	statement.sign_sr25519_private(keypair);
	statement
}

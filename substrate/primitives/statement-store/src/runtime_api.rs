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

//! Runtime support for the statement store.

use crate::{Hash, Statement, Topic};
use alloc::vec::Vec;
use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_runtime_interface::{
	pass_by::{
		AllocateAndReturnByCodec, PassFatPointerAndDecode, PassFatPointerAndDecodeSlice,
		PassPointerAndRead, PassPointerAndReadCopy, ReturnAs,
	},
	runtime_interface,
};
use Debug;

#[cfg(feature = "std")]
use sp_externalities::ExternalitiesExt;

/// The source of the statement.
///
/// Depending on the source we might apply different validation schemes.
#[derive(Copy, Clone, PartialEq, Eq, Encode, Decode, Debug, TypeInfo)]
pub enum StatementSource {
	/// Statement is coming from the on-chain worker.
	Chain,
	/// Statement has been received from the gossip network.
	Network,
	/// Statement has been submitted over the local api.
	Local,
}

impl StatementSource {
	/// Check if the source allows the statement to be resubmitted to the store, extending its
	/// expiration date.
	pub fn can_be_resubmitted(&self) -> bool {
		match self {
			StatementSource::Chain | StatementSource::Local => true,
			StatementSource::Network => false,
		}
	}
}

#[cfg(feature = "std")]
sp_externalities::decl_extension! {
	/// The offchain database extension that will be registered at the Substrate externalities.
	pub struct StatementStoreExt(std::sync::Arc<dyn crate::StatementStore>);
}

// Host extensions for the runtime.
#[cfg(feature = "std")]
impl StatementStoreExt {
	/// Create new instance of externalities extensions.
	pub fn new(store: std::sync::Arc<dyn crate::StatementStore>) -> Self {
		Self(store)
	}
}

/// Submission result.
#[derive(Debug, Eq, PartialEq, Clone, Copy, Encode, Decode)]
pub enum SubmitResult {
	/// Accepted as new.
	OkNew = 0,
	/// Known statement
	OkKnown = 1,
	/// Statement failed validation.
	Bad = 2,
	/// The store is not available.
	NotAvailable = 3,
	/// Statement could not be inserted because of priority or size checks.
	Full = 4,
}

impl TryFrom<u8> for SubmitResult {
	type Error = ();
	fn try_from(value: u8) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(SubmitResult::OkNew),
			1 => Ok(SubmitResult::OkKnown),
			2 => Ok(SubmitResult::Bad),
			3 => Ok(SubmitResult::NotAvailable),
			4 => Ok(SubmitResult::Full),
			_ => Err(()),
		}
	}
}

impl From<SubmitResult> for u8 {
	fn from(value: SubmitResult) -> Self {
		value as u8
	}
}

/// Export functions for the WASM host.
#[cfg(feature = "std")]
pub type HostFunctions = (statement_store::HostFunctions,);

/// Host interface
#[runtime_interface]
pub trait StatementStore {
	/// Submit a new statement. The statement will be broadcast to the network.
	/// This is meant to be used by the offchain worker.
	fn submit_statement(
		&mut self,
		statement: PassFatPointerAndDecode<Statement>,
	) -> ReturnAs<SubmitResult, u8> {
		if let Some(StatementStoreExt(store)) = self.extension::<StatementStoreExt>() {
			match store.submit(statement, StatementSource::Chain) {
				crate::SubmitResult::New => SubmitResult::OkNew,
				crate::SubmitResult::Known => SubmitResult::OkKnown,
				crate::SubmitResult::Rejected(_) => SubmitResult::Full,
				// This should not happen for `StatementSource::Chain`. An existing statement will
				// be overwritten.
				crate::SubmitResult::KnownExpired => SubmitResult::Bad,
				crate::SubmitResult::Invalid(_) => SubmitResult::Bad,
				crate::SubmitResult::InternalError(_) => SubmitResult::Bad,
			}
		} else {
			SubmitResult::NotAvailable
		}
	}

	/// Return all statements.
	fn statements(&mut self) -> AllocateAndReturnByCodec<Vec<(Hash, Statement)>> {
		if let Some(StatementStoreExt(store)) = self.extension::<StatementStoreExt>() {
			store.statements().unwrap_or_default()
		} else {
			Vec::default()
		}
	}

	/// Return the data of all known statements which include all topics and have no `DecryptionKey`
	/// field.
	fn broadcasts(
		&mut self,
		match_all_topics: PassFatPointerAndDecodeSlice<&[Topic]>,
	) -> AllocateAndReturnByCodec<Vec<Vec<u8>>> {
		if let Some(StatementStoreExt(store)) = self.extension::<StatementStoreExt>() {
			store.broadcasts(match_all_topics).unwrap_or_default()
		} else {
			Vec::default()
		}
	}

	/// Return the data of all known statements whose decryption key is identified as `dest` (this
	/// will generally be the public key or a hash thereof for symmetric ciphers, or a hash of the
	/// private key for symmetric ciphers).
	fn posted(
		&mut self,
		match_all_topics: PassFatPointerAndDecodeSlice<&[Topic]>,
		dest: PassPointerAndReadCopy<[u8; 32], 32>,
	) -> AllocateAndReturnByCodec<Vec<Vec<u8>>> {
		if let Some(StatementStoreExt(store)) = self.extension::<StatementStoreExt>() {
			store.posted(match_all_topics, dest).unwrap_or_default()
		} else {
			Vec::default()
		}
	}

	/// Return the decrypted data of all known statements whose decryption key is identified as
	/// `dest`. The key must be available to the client.
	fn posted_clear(
		&mut self,
		match_all_topics: PassFatPointerAndDecodeSlice<&[Topic]>,
		dest: PassPointerAndReadCopy<[u8; 32], 32>,
	) -> AllocateAndReturnByCodec<Vec<Vec<u8>>> {
		if let Some(StatementStoreExt(store)) = self.extension::<StatementStoreExt>() {
			store.posted_clear(match_all_topics, dest).unwrap_or_default()
		} else {
			Vec::default()
		}
	}

	/// Remove a statement from the store by hash.
	fn remove(&mut self, hash: PassPointerAndRead<&Hash, 32>) {
		if let Some(StatementStoreExt(store)) = self.extension::<StatementStoreExt>() {
			store.remove(hash).unwrap_or_default()
		}
	}

	/// Remove all statements from the store that were posted by the given public key.
	fn remove_by(&mut self, who: PassPointerAndReadCopy<[u8; 32], 32>) {
		if let Some(StatementStoreExt(store)) = self.extension::<StatementStoreExt>() {
			let _ = store.remove_by(who);
		}
	}
}

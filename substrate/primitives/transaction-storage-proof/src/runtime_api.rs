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

//! Runtime API definition for the transaction storage proof processing.

use alloc::vec::Vec;
use sp_runtime::traits::NumberFor;

sp_api::decl_runtime_apis! {
	/// Runtime API trait for transaction storage support.
	#[api_version(2)]
	pub trait TransactionStorageApi {
		/// Retention period for indexed data, in blocks.
		fn retention_period() -> NumberFor<Block>;

		/// Indexed-transaction metadata for `block`.
		///
		/// Returns an empty vector if the block has no indexed transactions or
		/// is outside the retention window.
		fn indexed_transactions(block: NumberFor<Block>) -> Vec<crate::IndexedTransactionInfo>;
	}
}

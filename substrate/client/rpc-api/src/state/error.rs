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

//! State RPC errors.

use jsonrpsee::types::error::{ErrorObject, ErrorObjectOwned};

/// State RPC Result type.
pub type Result<T> = std::result::Result<T, Error>;

/// State RPC errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// Client error.
	#[error("Client error: {}", .0)]
	Client(#[from] Box<dyn std::error::Error + Send + Sync>),
	/// Provided block range couldn't be resolved to a list of blocks.
	#[error("Cannot resolve a block range ['{:?}' ... '{:?}]. {}", .from, .to, .details)]
	InvalidBlockRange {
		/// Beginning of the block range.
		from: String,
		/// End of the block range.
		to: String,
		/// Details of the error message.
		details: String,
	},
	/// Provided count exceeds maximum value.
	#[error("count exceeds maximum value. value: {}, max: {}", .value, .max)]
	InvalidCount {
		/// Provided value
		value: u32,
		/// Maximum allowed value
		max: u32,
	},
	/// Call to an unsafe RPC was denied.
	#[error(transparent)]
	UnsafeRpcCalled(#[from] crate::policy::UnsafeRpcError),
	/// The node registers no proof-size recorder and so cannot service a recorded runtime call.
	#[error("Recorded runtime calls are not supported by this node")]
	CallRecordedUnsupported,
	/// A recorded runtime call was denied because unsafe RPC methods are disabled on this node.
	#[error("Recorded runtime calls are unsafe and disabled on this node")]
	CallRecordedDenied,
}

/// Base code for all state errors.
const BASE_ERROR: i32 = crate::error::base::STATE;

/// Error code for [`Error::CallRecordedUnsupported`]. Stable wire contract matched by clients to
/// decide fallback; do not renumber.
pub const CALL_RECORDED_UNSUPPORTED_ERROR_CODE: i32 = BASE_ERROR + 4;

/// Error code for [`Error::CallRecordedDenied`]. Stable wire contract matched by clients to decide
/// fallback; do not renumber.
pub const CALL_RECORDED_DENIED_ERROR_CODE: i32 = BASE_ERROR + 5;

impl From<Error> for ErrorObjectOwned {
	fn from(e: Error) -> ErrorObjectOwned {
		match e {
			Error::InvalidBlockRange { .. } => {
				ErrorObject::owned(BASE_ERROR + 1, e.to_string(), None::<()>)
			},
			Error::InvalidCount { .. } => {
				ErrorObject::owned(BASE_ERROR + 2, e.to_string(), None::<()>)
			},
			Error::CallRecordedUnsupported => {
				ErrorObject::owned(CALL_RECORDED_UNSUPPORTED_ERROR_CODE, e.to_string(), None::<()>)
			},
			Error::CallRecordedDenied => {
				ErrorObject::owned(CALL_RECORDED_DENIED_ERROR_CODE, e.to_string(), None::<()>)
			},
			e => ErrorObject::owned(BASE_ERROR + 3, e.to_string(), None::<()>),
		}
	}
}

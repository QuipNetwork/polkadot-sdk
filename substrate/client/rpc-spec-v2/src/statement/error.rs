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

use jsonrpsee::types::error::ErrorObject;

/// Error returned by statement RPC methods
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// The subscription id is not active on this connection
	#[error("Invalid statement subscription identifier")]
	InvalidSubscription,
	/// A request parameter is invalid
	#[error("Invalid parameter: {0}")]
	InvalidParam(String),
	/// The server failed while handling a valid request
	#[error("Internal error: {0}")]
	InternalError(String),
}

/// Error codes defined by the statement RPC specification.
pub mod rpc_spec_v2 {
	/// Subscription identifier is invalid
	pub const INVALID_SUBSCRIPTION: i32 = -32801;
}

/// Error codes defined by the JSON-RPC specification.
pub mod json_rpc_spec {
	/// Request parameters are invalid
	pub const INVALID_PARAM_ERROR: i32 = -32602;
	/// Internal server error occurs
	pub const INTERNAL_ERROR: i32 = -32603;
}

impl From<Error> for ErrorObject<'static> {
	fn from(e: Error) -> Self {
		let msg = e.to_string();
		match e {
			Error::InvalidSubscription => {
				ErrorObject::owned(rpc_spec_v2::INVALID_SUBSCRIPTION, msg, None::<()>)
			},
			Error::InvalidParam(_) => {
				ErrorObject::owned(json_rpc_spec::INVALID_PARAM_ERROR, msg, None::<()>)
			},
			Error::InternalError(_) => {
				ErrorObject::owned(json_rpc_spec::INTERNAL_ERROR, msg, None::<()>)
			},
		}
	}
}

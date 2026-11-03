// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.
// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

use std::net::AddrParseError;
use thiserror::Error;

/// Result type used by relay utilities.
pub type Result<T> = std::result::Result<T, Error>;

/// Relay utilities errors.
#[derive(Error, Debug)]
pub enum Error {
	/// Invalid host address was used for exposing Prometheus metrics.
	#[error("Invalid host {0} is used to expose Prometheus metrics: {1}")]
	ExposingMetricsInvalidHost(String, AddrParseError),
	/// Prometheus error.
	#[error("{0}")]
	Prometheus(#[from] prometheus_endpoint::prometheus::Error),
}

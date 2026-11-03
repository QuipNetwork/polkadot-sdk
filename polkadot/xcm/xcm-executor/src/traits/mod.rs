// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Various traits used in configuring the executor.

mod conversion;
pub use conversion::{CallDispatcher, ConvertLocation, ConvertOrigin, WithOriginFilter};
mod drop_assets;
pub use drop_assets::{ClaimAssets, DropAssets, TrapAndClaimAssets};
mod asset_exchange;
pub use asset_exchange::AssetExchange;
mod asset_lock;
pub use asset_lock::{AssetLock, Enact, LockError};
mod asset_transfer;
pub use asset_transfer::{Error as AssetTransferError, TransferType, XcmAssetTransfers};
mod export;
pub use export::{export_xcm, validate_export, ExportXcm};
mod fee_manager;
pub use fee_manager::{FeeManager, FeeReason, WaiveDeliveryFees};
mod token_matching;
pub use token_matching::{
	Error, MatchesFungible, MatchesFungibles, MatchesInstance, MatchesNonFungible,
	MatchesNonFungibles,
};
mod on_response;
pub use on_response::{OnResponse, QueryHandler, QueryResponseStatus, VersionChangeNotifier};
mod process_transaction;
pub use process_transaction::ProcessTransaction;
mod should_execute;
pub use should_execute::{CheckSuspension, DenyExecution, Properties, ShouldExecute};
mod transact_asset;
pub use transact_asset::TransactAsset;
mod hrmp;
pub use hrmp::{
	HandleHrmpChannelAccepted, HandleHrmpChannelClosing, HandleHrmpNewChannelOpenRequest,
};
mod event_emitter;
mod record_xcm;
mod weight;
pub use event_emitter::EventEmitter;

pub use record_xcm::RecordXcm;
pub use weight::{WeightBounds, WeightTrader};

pub mod prelude {
	pub use super::{
		export_xcm, validate_export, AssetExchange, AssetLock, ClaimAssets, ConvertOrigin,
		DropAssets, Enact, Error, EventEmitter, ExportXcm, FeeManager, FeeReason, LockError,
		MatchesFungible, MatchesFungibles, MatchesInstance, MatchesNonFungible,
		MatchesNonFungibles, OnResponse, ProcessTransaction, ShouldExecute, TransactAsset,
		TrapAndClaimAssets, VersionChangeNotifier, WeightBounds, WeightTrader, WithOriginFilter,
	};
}

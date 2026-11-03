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

//! Traits and associated utilities for use in the FRAME environment.
//!
//! NOTE: If you're looking for `parameter_types`, it has moved in to the top-level module.

pub mod tokens;
pub use tokens::{
	currency::{
		ActiveIssuanceOf, Currency, InspectLockableCurrency, LockIdentifier, LockableCurrency,
		NamedReservableCurrency, ReservableCurrency, TotalIssuanceOf, VestedTransfer,
		VestingSchedule,
	},
	fungible, fungibles,
	imbalance::{Imbalance, OnUnbalanced, SignedImbalance},
	nonfungible, nonfungible_v2, nonfungibles, nonfungibles_v2, BalanceStatus,
	ExistenceRequirement, Locker, WithdrawReasons,
};

mod members;
pub use members::{
	AsContains, ChangeMembers, Contains, ContainsLengthBound, ContainsPair, Equals, Everything,
	EverythingBut, FromContains, FromContainsPair, InitializeMembers, InsideBoth, IsInVec, Nothing,
	RankedMembers, RankedMembersSwapHandler, SortedMembers, TheseExcept,
};

mod validation;
pub use validation::{
	DisabledValidators, EstimateNextNewSession, EstimateNextSessionRotation, FindAuthor,
	KeyOwnerProofSystem, Lateness, OneSessionHandler, ValidatorRegistration, ValidatorSet,
	ValidatorSetWithIdentification, VerifySeal,
};

mod error;
pub use error::PalletError;

mod filter;
pub use filter::{ClearFilterGuard, FilterStack, FilterStackGuard, InstanceFilter};

mod misc;
pub use misc::{
	defensive_prelude::{self, *},
	AccountTouch, Backing, ConstBool, ConstI128, ConstI16, ConstI32, ConstI64, ConstI8, ConstInt,
	ConstU128, ConstU16, ConstU32, ConstU64, ConstU8, ConstUint, DefensiveMax, DefensiveMin,
	DefensiveSaturating, DefensiveTruncateFrom, DefensiveTruncateInto, EqualPrivilegeOnly,
	EstimateCallFee, EstimateFee, ExecuteBlock, Get, GetBacking, GetDefault, HandleLifetime,
	InherentBuilder, IsInherent, IsSubType, IsType, Len, OffchainWorker, OnKilledAccount,
	OnNewAccount, PrivilegeCmp, RewardsReporter, SameOrOther, SignedTransactionBuilder, Time,
	TryCollect, TryDrop, TypedGet, UnixTime, VariantCount, VariantCountOf, WrapperKeepOpaque,
	WrapperOpaque,
};
#[allow(deprecated)]
pub use misc::{PreimageProvider, PreimageRecipient};
#[doc(hidden)]
pub use misc::{DEFENSIVE_OP_INTERNAL_ERROR, DEFENSIVE_OP_PUBLIC_ERROR};

mod stored_map;
pub use stored_map::{StorageMapShim, StoredMap};
mod randomness;
pub use randomness::Randomness;
pub mod reality;

mod metadata;
pub use metadata::{
	CallMetadata, CrateVersion, GetCallIndex, GetCallMetadata, GetCallName, GetStorageVersion,
	NoStorageVersionSet, PalletInfo, PalletInfoAccess, PalletInfoData, PalletsInfoAccess,
	StorageVersion, STORAGE_VERSION_STORAGE_KEY_POSTFIX,
};

mod hooks;
pub use hooks::{
	BeforeAllRuntimeMigrations, BuildGenesisConfig, Hooks, IntegrityTest, OnFinalize, OnGenesis,
	OnIdle, OnInitialize, OnPoll, OnRuntimeUpgrade, OnTimestampSet, PostInherents,
	PostTransactions, PreInherents, UncheckedOnRuntimeUpgrade,
};

pub mod schedule;
mod storage;
pub use storage::{
	Consideration, ConstantStoragePrice, Disabled, Footprint, Incrementable, Instance,
	LinearStoragePrice, MaybeConsideration, NoDrop, PartialStorageInfoTrait, StorageInfo,
	StorageInfoTrait, StorageInstance, SuppressedDrop, TrackedStorageKey, WhitelistedStorageKeys,
};

mod dispatch;
pub use dispatch::{
	AsEnsureOriginWithArg, Authorize, CallerTrait, EitherOf, EitherOfDiverse, EnsureOrigin,
	EnsureOriginEqualOrHigherPrivilege, EnsureOriginWithArg, MapSuccess, NeverEnsureOrigin,
	OriginTrait, TryMapSuccess, TryWithMorphedArg, UnfilteredDispatchable,
};

mod voting;
pub use voting::{ClassCountOf, NoOpPoll, PollStatus, Polling, VoteTally};

mod preimages;
pub use preimages::{Bounded, BoundedInline, FetchResult, QueryPreimage, StorePreimage};

mod messages;
pub use messages::{
	BatchFootprint, BatchesFootprints, EnqueueMessage, EnqueueWithOrigin, ExecuteOverweightError,
	HandleMessage, NoopServiceQueues, ProcessMessage, ProcessMessageError, QueueFootprint,
	QueueFootprintQuery, QueuePausedQuery, ServiceQueues, TransformOrigin,
};

mod safe_mode;
pub use safe_mode::{SafeMode, SafeModeError, SafeModeNotify};

mod tx_pause;
pub use tx_pause::{TransactionPause, TransactionPauseError};

pub mod dynamic_params;

pub mod tasks;
pub use tasks::Task;

mod proving;
pub use proving::*;

mod rewards;
pub use rewards::RewardsPool;

#[cfg(feature = "try-runtime")]
mod try_runtime;
#[cfg(feature = "try-runtime")]
pub use try_runtime::{
	Select as TryStateSelect, TryDecodeEntireStorage, TryDecodeEntireStorageError, TryState,
	UpgradeCheckSelect,
};

// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! A pallet which uses the XCMP transport layer to handle both incoming and outgoing XCM message
//! sending and dispatch, queuing, signalling and backpressure. To do so, it implements:
//! * `XcmpMessageHandler`
//! * `XcmpMessageSource`
//!
//! Also provides an implementation of `SendXcm` which can be placed in a router tuple for relaying
//! XCM over XCMP if the destination is `Parent/Parachain`. It requires an implementation of
//! `XcmExecutor` for dispatching incoming XCM messages.
//!
//! To prevent out of memory errors on the `OutboundXcmpMessages` queue, an exponential fee factor
//! (`DeliveryFeeFactor`) is set, much like the one used in DMP.
//! The fee factor increases whenever the total size of messages in a particular channel passes a
//! threshold. This threshold is defined as a percentage of the maximum total size the channel can
//! have. More concretely, the threshold is `max_total_size` / `THRESHOLD_FACTOR`, where:
//! - `max_total_size` is the maximum size, in bytes, of the channel, not number of messages.
//! It is defined in the channel configuration.
//! - `THRESHOLD_FACTOR` just declares which percentage of the max size is the actual threshold.
//! If it's 2, then the threshold is half of the max size, if it's 4, it's a quarter, and so on.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod migration;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(feature = "bridging")]
pub mod bridging;
pub mod weights;
pub mod weights_ext;

pub use weights::WeightInfo;
pub use weights_ext::WeightInfoExt;

extern crate alloc;

use alloc::{collections::BTreeSet, vec, vec::Vec};
use bitflags::bitflags;
use bounded_collections::{BoundedBTreeSet, BoundedSlice, BoundedVec};
use codec::{Compact, Decode, DecodeLimit, Encode, MaxEncodedLen};
use cumulus_primitives_core::{
	relay_chain::BlockNumber as RelayBlockNumber, ChannelStatus, GetChannelInfo, MessageSendError,
	ParaId, XcmpMessageFormat, XcmpMessageHandler, XcmpMessageSource,
};

use frame_support::{
	defensive, defensive_assert,
	traits::{
		Defensive, DefensiveTruncateFrom, EnqueueMessage, EnsureOrigin, Get, Len, QueueFootprint,
		QueueFootprintQuery, QueuePausedQuery,
	},
	weights::{Weight, WeightMeter},
};
use pallet_message_queue::OnQueueChanged;
use polkadot_runtime_common::xcm_sender::PriceForMessageDelivery;
use polkadot_runtime_parachains::{FeeTracker, GetMinFeeFactor};
use scale_info::TypeInfo;
use sp_core::MAX_POSSIBLE_ALLOCATION;
use sp_runtime::{FixedU128, SaturatedConversion, WeakBoundedVec};
use xcm::{latest::prelude::*, VersionedLocation, VersionedXcm, WrapVersion, MAX_XCM_DECODE_DEPTH};
use xcm_builder::InspectMessageQueues;
use xcm_executor::traits::ConvertOrigin;

pub use pallet::*;

/// Index used to identify overweight XCMs.
pub type OverweightIndex = u64;
/// The max length of an XCMP message.
pub type MaxXcmpMessageLenOf<T> =
	<<T as Config>::XcmpQueue as EnqueueMessage<ParaId>>::MaxMessageLen;

const LOG_TARGET: &str = "xcmp_queue";
const DEFAULT_POV_SIZE: u64 = 64 * 1024; // 64 KB
/// The size of an XCM messages batch.
pub const XCM_BATCH_SIZE: usize = 250;
/// The maximum number of signals that we can have in an XCMP page.
pub const MAX_SIGNALS_PER_PAGE: usize = 3;

/// Constants related to delivery fee calculation
pub mod delivery_fee_constants {
	/// Fees will start increasing when queue is half full
	pub const THRESHOLD_FACTOR: u32 = 2;
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::{pallet_prelude::*, Twox64Concat};
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	#[pallet::storage_version(migration::STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Information on the available XCMP channels.
		type ChannelInfo: GetChannelInfo;

		/// Means of converting an `Xcm` into a `VersionedXcm`.
		type VersionWrapper: WrapVersion;

		/// Enqueue an inbound horizontal message for later processing.
		///
		/// This defines the maximal message length via [`crate::MaxXcmpMessageLenOf`]. The pallet
		/// assumes that this hook will eventually process all the pushed messages.
		type XcmpQueue: EnqueueMessage<ParaId>
			+ QueueFootprintQuery<ParaId, MaxMessageLen = MaxXcmpMessageLenOf<Self>>;

		/// The maximum number of inbound XCMP channels that can be suspended simultaneously.
		///
		/// Any further channel suspensions will fail and messages may get dropped without further
		/// notice. Choosing a high value (1000) is okay; the trade-off that is described in
		/// [`InboundXcmpSuspended`] still applies at that scale.
		#[pallet::constant]
		type MaxInboundSuspended: Get<u32>;

		/// Maximal number of outbound XCMP channels that can have messages queued at the same time.
		///
		/// If this is reached, then no further messages can be sent to channels that do not yet
		/// have a message queued. This should be set to the expected maximum of outbound channels
		/// which is determined by [`Self::ChannelInfo`]. It is important to set this large enough,
		/// since otherwise the congestion control protocol will not work as intended and messages
		/// may be dropped. This value increases the PoV and should therefore not be picked too
		/// high. Governance needs to pay attention to not open more channels than this value.
		#[pallet::constant]
		type MaxActiveOutboundChannels: Get<u32>;

		/// The maximal page size for HRMP message pages.
		///
		/// A lower limit can be set dynamically, but this is the hard-limit for the PoV worst case
		/// benchmarking. The limit for the size of a message is slightly below this, since some
		/// overhead is incurred for encoding the format.
		#[pallet::constant]
		type MaxPageSize: Get<u32>;

		/// The origin that is allowed to resume or suspend the XCMP queue.
		type ControllerOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The conversion function used to attempt to convert an XCM `Location` origin to a
		/// superuser origin.
		type ControllerOriginConverter: ConvertOrigin<Self::RuntimeOrigin>;

		/// The price for delivering an XCM to a sibling parachain destination.
		type PriceForSiblingDelivery: PriceForMessageDelivery<Id = ParaId>;

		/// The weight information of this pallet.
		type WeightInfo: WeightInfoExt;
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Suspends all XCM executions for the XCMP queue, regardless of the sender's origin.
		///
		/// - `origin`: Must pass `ControllerOrigin`.
		#[pallet::call_index(1)]
		#[pallet::weight((T::DbWeight::get().writes(1), DispatchClass::Operational,))]
		pub fn suspend_xcm_execution(origin: OriginFor<T>) -> DispatchResult {
			T::ControllerOrigin::ensure_origin(origin)?;

			QueueSuspended::<T>::try_mutate(|suspended| {
				if *suspended {
					Err(Error::<T>::AlreadySuspended.into())
				} else {
					*suspended = true;
					Ok(())
				}
			})
		}

		/// Resumes all XCM executions for the XCMP queue.
		///
		/// Note that this function doesn't change the status of the in/out bound channels.
		///
		/// - `origin`: Must pass `ControllerOrigin`.
		#[pallet::call_index(2)]
		#[pallet::weight((T::DbWeight::get().writes(1), DispatchClass::Operational,))]
		pub fn resume_xcm_execution(origin: OriginFor<T>) -> DispatchResult {
			T::ControllerOrigin::ensure_origin(origin)?;

			QueueSuspended::<T>::try_mutate(|suspended| {
				if !*suspended {
					Err(Error::<T>::AlreadyResumed.into())
				} else {
					*suspended = false;
					Ok(())
				}
			})
		}

		/// Overwrites the number of pages which must be in the queue for the other side to be
		/// told to suspend their sending.
		///
		/// - `origin`: Must pass `Root`.
		/// - `new`: Desired value for `QueueConfigData.suspend_value`
		#[pallet::call_index(3)]
		#[pallet::weight((T::WeightInfo::set_config_with_u32(), DispatchClass::Operational,))]
		pub fn update_suspend_threshold(origin: OriginFor<T>, new: u32) -> DispatchResult {
			ensure_root(origin)?;

			QueueConfig::<T>::try_mutate(|data| {
				data.suspend_threshold = new;
				data.validate::<T>()
			})
		}

		/// Overwrites the number of pages which must be in the queue after which we drop any
		/// further messages from the channel.
		///
		/// - `origin`: Must pass `Root`.
		/// - `new`: Desired value for `QueueConfigData.drop_threshold`
		#[pallet::call_index(4)]
		#[pallet::weight((T::WeightInfo::set_config_with_u32(),DispatchClass::Operational,))]
		pub fn update_drop_threshold(origin: OriginFor<T>, new: u32) -> DispatchResult {
			ensure_root(origin)?;

			QueueConfig::<T>::try_mutate(|data| {
				data.drop_threshold = new;
				data.validate::<T>()
			})
		}

		/// Overwrites the number of pages which the queue must be reduced to before it signals
		/// that message sending may recommence after it has been suspended.
		///
		/// - `origin`: Must pass `Root`.
		/// - `new`: Desired value for `QueueConfigData.resume_threshold`
		#[pallet::call_index(5)]
		#[pallet::weight((T::WeightInfo::set_config_with_u32(), DispatchClass::Operational,))]
		pub fn update_resume_threshold(origin: OriginFor<T>, new: u32) -> DispatchResult {
			ensure_root(origin)?;

			QueueConfig::<T>::try_mutate(|data| {
				data.resume_threshold = new;
				data.validate::<T>()
			})
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			assert!(!T::MaxPageSize::get().is_zero(), "MaxPageSize too low");

			let w = Self::on_idle_weight();
			assert!(w != Weight::zero());
			assert!(w.all_lte(T::BlockWeights::get().max_block));

			<T::WeightInfo as WeightInfoExt>::check_accuracy::<MaxXcmpMessageLenOf<T>>(0.15);
		}

		fn on_idle(_block: BlockNumberFor<T>, limit: Weight) -> Weight {
			let mut meter = WeightMeter::with_limit(limit);

			if meter.try_consume(Self::on_idle_weight()).is_err() {
				tracing::debug!(
					target: LOG_TARGET,
					"Not enough weight for on_idle. {} < {}",
					Self::on_idle_weight(), limit
				);
				return meter.consumed();
			}

			migration::v3::lazy_migrate_inbound_queue::<T>();

			meter.consumed()
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// An HRMP message was sent to a sibling parachain.
		XcmpMessageSent { message_hash: XcmHash },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Setting the queue config failed since one of its values was invalid.
		BadQueueConfig,
		/// The execution is already suspended.
		AlreadySuspended,
		/// The execution is already resumed.
		AlreadyResumed,
		/// There are too many active outbound channels.
		TooManyActiveOutboundChannels,
		/// The message is too big.
		TooBig,
	}

	/// The suspended inbound XCMP channels. All others are not suspended.
	///
	/// This is a `StorageValue` instead of a `StorageMap` since we expect multiple reads per block
	/// to different keys with a one byte payload. The access to `BoundedBTreeSet` will be cached
	/// within the block and therefore only included once in the proof size.
	///
	/// NOTE: The PoV benchmarking cannot know this and will over-estimate, but the actual proof
	/// will be smaller.
	#[pallet::storage]
	pub type InboundXcmpSuspended<T: Config> =
		StorageValue<_, BoundedBTreeSet<ParaId, T::MaxInboundSuspended>, ValueQuery>;

	/// The non-empty XCMP channels in order of becoming non-empty, and the index of the first
	/// and last outbound message. If the two indices are equal, then it indicates an empty
	/// queue and there must be a non-`Ok` `OutboundStatus`. We assume queues grow no greater
	/// than 65535 items. Queue indices for normal messages begin at one; zero is reserved in
	/// case of the need to send a high-priority signal message this block.
	/// The bool is true if there is a signal message waiting to be sent.
	#[pallet::storage]
	pub(super) type OutboundXcmpStatus<T: Config> = StorageValue<
		_,
		BoundedVec<OutboundChannelDetails, T::MaxActiveOutboundChannels>,
		ValueQuery,
	>;

	/// The messages outbound in a given XCMP channel.
	#[pallet::storage]
	pub(super) type OutboundXcmpMessages<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		ParaId,
		Twox64Concat,
		u16,
		WeakBoundedVec<u8, T::MaxPageSize>,
		ValueQuery,
	>;

	/// Any signal messages waiting to be sent.
	#[pallet::storage]
	pub(super) type SignalMessages<T: Config> =
		StorageMap<_, Blake2_128Concat, ParaId, WeakBoundedVec<u8, T::MaxPageSize>, ValueQuery>;

	/// The configuration which controls the dynamics of the outbound queue.
	#[pallet::storage]
	pub(super) type QueueConfig<T: Config> = StorageValue<_, QueueConfigData, ValueQuery>;

	/// Whether or not the XCMP queue is suspended from executing incoming XCMs or not.
	#[pallet::storage]
	pub(super) type QueueSuspended<T: Config> = StorageValue<_, bool, ValueQuery>;

	/// The factor to multiply the base delivery fee by.
	#[pallet::storage]
	pub(super) type DeliveryFeeFactor<T: Config> =
		StorageMap<_, Twox64Concat, ParaId, FixedU128, ValueQuery, GetMinFeeFactor<Pallet<T>>>;
}

#[derive(Copy, Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo, MaxEncodedLen)]
pub enum OutboundState {
	Ok,
	Suspended,
}

bitflags! {
	#[derive(Encode, Decode, TypeInfo, MaxEncodedLen)]
	struct OutboundChannelFlags: u32 {
		const CONCATENATED_OPAQUE_VERSIONED_XCM_SUPPORT = 1;
		const CONCATENATED_OPAQUE_VERSIONED_XCM_NOTIFICATION_SENT = 1 << 1;
	}
}

impl OutboundChannelFlags {
	// Check whether the recipient supports `ConcatenatedOpaqueVersionedXcm`.
	fn has_concatenated_opaque_versioned_xcm_support(&self) -> bool {
		*self & Self::CONCATENATED_OPAQUE_VERSIONED_XCM_SUPPORT != Self::empty()
	}

	// Check whether we should send a notification to the recipient, advertising that we support
	// `ConcatenatedOpaqueVersionedXcm`.
	fn should_send_concatenated_opaque_versioned_xcm_notification(&self) -> bool {
		if self.has_concatenated_opaque_versioned_xcm_support() {
			return false;
		}

		if *self & Self::CONCATENATED_OPAQUE_VERSIONED_XCM_NOTIFICATION_SENT != Self::empty() {
			return false;
		}

		true
	}

	// Remember that the recipient supports `ConcatenatedOpaqueVersionedXcm`.
	fn notice_concatenated_opaque_versioned_xcm_support(&mut self) {
		*self = *self | Self::CONCATENATED_OPAQUE_VERSIONED_XCM_SUPPORT;
	}

	// Remember that we advertised the `ConcatenatedOpaqueVersionedXcm` support to the recipient.
	fn notice_concatenated_opaque_versioned_xcm_notification_sent(&mut self) {
		*self = *self | Self::CONCATENATED_OPAQUE_VERSIONED_XCM_NOTIFICATION_SENT;
	}
}

/// Struct containing detailed information about the outbound channel.
#[derive(Clone, Eq, PartialEq, Encode, Decode, TypeInfo, Debug, MaxEncodedLen)]
pub struct OutboundChannelDetails {
	/// The `ParaId` of the parachain that this channel is connected with.
	recipient: ParaId,
	/// The state of the channel.
	state: OutboundState,
	/// Whether any signals exist in this channel.
	signals_exist: bool,
	/// The index of the first outbound message.
	first_index: u16,
	/// The index of the last outbound message.
	last_index: u16,
	/// Flags
	flags: OutboundChannelFlags,
	/// Cached total byte size of the pages currently queued in this channel.
	queued_bytes: u32,
}

impl OutboundChannelDetails {
	pub fn new(recipient: ParaId) -> OutboundChannelDetails {
		OutboundChannelDetails {
			recipient,
			state: OutboundState::Ok,
			signals_exist: false,
			first_index: 0,
			last_index: 0,
			flags: OutboundChannelFlags::empty(),
			queued_bytes: 0,
		}
	}

	pub fn with_signals(mut self) -> OutboundChannelDetails {
		self.signals_exist = true;
		self
	}

	pub fn with_suspended_state(mut self) -> OutboundChannelDetails {
		self.state = OutboundState::Suspended;
		self
	}
}

#[derive(Copy, Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo, MaxEncodedLen)]
pub struct QueueConfigData {
	/// The number of pages which must be in the queue for the other side to be told to suspend
	/// their sending.
	suspend_threshold: u32,
	/// The number of pages which must be in the queue after which we drop any further messages
	/// from the channel. This should normally not happen since the `suspend_threshold` can be used
	/// to suspend the channel.
	drop_threshold: u32,
	/// The number of pages which the queue must be reduced to before it signals that
	/// message sending may recommence after it has been suspended.
	resume_threshold: u32,
}

impl Default for QueueConfigData {
	fn default() -> Self {
		// NOTE that these default values are only used on genesis. They should give a rough idea of
		// what to set these values to, but is in no way a requirement.
		Self {
			drop_threshold: 48,    // 64KiB * 48 = 3MiB
			suspend_threshold: 32, // 64KiB * 32 = 2MiB
			resume_threshold: 8,   // 64KiB * 8 = 512KiB
		}
	}
}

impl QueueConfigData {
	/// Validate all assumptions about `Self`.
	///
	/// Should be called prior to accepting this as new config.
	pub fn validate<T: crate::Config>(&self) -> sp_runtime::DispatchResult {
		if self.resume_threshold < self.suspend_threshold &&
			self.suspend_threshold <= self.drop_threshold &&
			self.resume_threshold > 0
		{
			Ok(())
		} else {
			Err(Error::<T>::BadQueueConfig.into())
		}
	}
}

#[derive(PartialEq, Eq, Copy, Clone, Encode, Decode, TypeInfo)]
pub enum ChannelSignal {
	Suspend,
	Resume,
}

impl<T: Config> Pallet<T> {
	fn try_get_outbound_channel(
		all_channels: &BoundedVec<OutboundChannelDetails, T::MaxActiveOutboundChannels>,
		recipient: ParaId,
	) -> Option<&OutboundChannelDetails> {
		for channel_idx in 0..all_channels.len() {
			if all_channels[channel_idx].recipient == recipient {
				return Some(&all_channels[channel_idx]);
			}
		}

		None
	}

	fn try_get_or_insert_outbound_channel(
		all_channels: &mut BoundedVec<OutboundChannelDetails, T::MaxActiveOutboundChannels>,
		recipient: ParaId,
	) -> Option<&mut OutboundChannelDetails> {
		for channel_idx in 0..all_channels.len() {
			if all_channels[channel_idx].recipient == recipient {
				return Some(&mut all_channels[channel_idx]);
			}
		}

		all_channels
			.try_push(OutboundChannelDetails::new(recipient))
			.inspect_err(|e| {
				tracing::error!(target: LOG_TARGET, error=?e, "Failed to insert outbound HRMP channel");
			})
			.ok()?;
		all_channels.last_mut()
	}

	/// Place a message `fragment` on the outgoing XCMP queue for `recipient`.
	///
	/// Format is the type of aggregate message that the `fragment` may be safely encoded and
	/// appended onto.
	///
	/// ## Background
	///
	/// For our purposes, one HRMP "message" is actually an aggregated block of XCM "messages".
	///
	/// For the sake of clarity, we distinguish between them as message AGGREGATEs versus
	/// message FRAGMENTs.
	///
	/// So each AGGREGATE is comprised of one or more concatenated SCALE-encoded `Vec<u8>`
	/// FRAGMENTs. Though each fragment is already probably a SCALE-encoded Xcm, we can't be
	/// certain, so we SCALE encode each `Vec<u8>` fragment in order to ensure we have the
	/// length prefixed and can thus decode each fragment from the aggregate stream. With this,
	/// we can concatenate them into a single aggregate blob without needing to be concerned
	/// about encoding fragment boundaries.
	///
	/// If successful, returns the number of pages in the outbound queue after enqueuing the new
	/// fragment.
	fn send_fragment<Fragment: Encode>(
		recipient: ParaId,
		format: XcmpMessageFormat,
		fragment: Fragment,
	) -> Result<u32, MessageSendError> {
		let mut encoded_fragment = fragment.encode();
		let encoded_fragment_len = encoded_fragment.len();

		// Optimization note: `max_message_size` could potentially be stored in
		// `OutboundXcmpMessages` once known; that way it's only accessed when a new page is needed.

		let channel_info =
			T::ChannelInfo::get_channel_info(recipient).ok_or(MessageSendError::NoChannel)?;
		// Max message size refers to aggregates, or pages. Not to individual fragments.
		let max_message_size = channel_info.max_message_size.min(T::MaxPageSize::get()) as usize;
		let format_size = format.encoded_size();
		// We check the encoded fragment length plus the format size against the max message size
		// because the format is concatenated if a new page is needed.
		let size_to_check = encoded_fragment
			.len()
			.checked_add(format_size)
			.ok_or(MessageSendError::TooBig)?;
		if size_to_check > max_message_size {
			return Err(MessageSendError::TooBig);
		}

		let mut all_channels = <OutboundXcmpStatus<T>>::get();
		let channel_details =
			Self::try_get_or_insert_outbound_channel(&mut all_channels, recipient)
				.ok_or(MessageSendError::TooManyChannels)?;
		if let XcmpMessageFormat::ConcatenatedOpaqueVersionedXcm = format {
			channel_details
				.flags
				.notice_concatenated_opaque_versioned_xcm_notification_sent();
		}

		let mut existing_page = None;
		'existing_page_check: {
			if channel_details.last_index > channel_details.first_index {
				let page =
					OutboundXcmpMessages::<T>::get(recipient, channel_details.last_index - 1);
				if XcmpMessageFormat::decode(&mut &page[..]) != Ok(format) {
					break 'existing_page_check;
				}
				if page.len() + encoded_fragment.len() > max_message_size {
					break 'existing_page_check;
				}
				existing_page = Some(page.into_inner());
			}
		}
		let mut current_page = existing_page.unwrap_or_else(|| {
			// We need to add a new page.
			channel_details.last_index += 1;
			let new_page = format.encode();
			channel_details.queued_bytes =
				channel_details.queued_bytes.saturating_add(new_page.len() as u32);
			new_page
		});

		current_page.append(&mut encoded_fragment);
		channel_details.queued_bytes =
			channel_details.queued_bytes.saturating_add(encoded_fragment_len as u32);
		let current_page = WeakBoundedVec::try_from(current_page).map_err(|error| {
			tracing::debug!(target: LOG_TARGET, ?error, "Failed to create bounded message page");
			MessageSendError::TooBig
		})?;
		let page_count =
			channel_details.last_index.saturating_sub(channel_details.first_index) as u32;
		<OutboundXcmpMessages<T>>::insert(recipient, channel_details.last_index - 1, current_page);

		let threshold = channel_info.max_total_size / delivery_fee_constants::THRESHOLD_FACTOR;
		if channel_details.queued_bytes > threshold {
			Self::increase_fee_factor(recipient, encoded_fragment_len as u128);
		}

		<OutboundXcmpStatus<T>>::put(all_channels);

		Ok(page_count)
	}

	/// Sends a signal to the `dest` chain over XCMP. This is guaranteed to be dispatched on this
	/// block.
	fn send_signal(dest: ParaId, signal: ChannelSignal) -> Result<(), Error<T>> {
		let mut s = <OutboundXcmpStatus<T>>::get();
		if let Some(details) = s.iter_mut().find(|item| item.recipient == dest) {
			details.signals_exist = true;
		} else {
			s.try_push(OutboundChannelDetails::new(dest).with_signals()).map_err(|error| {
				tracing::debug!(target: LOG_TARGET, ?error, "Failed to activate XCMP channel");
				Error::<T>::TooManyActiveOutboundChannels
			})?;
		}

		let page = BoundedVec::<u8, T::MaxPageSize>::try_from(
			(XcmpMessageFormat::Signals, signal).encode(),
		)
		.map_err(|error| {
			tracing::debug!(target: LOG_TARGET, ?error, "Failed to encode signal message");
			Error::<T>::TooBig
		})?;
		let page = WeakBoundedVec::force_from(page.into_inner(), None);

		<SignalMessages<T>>::insert(dest, page);
		<OutboundXcmpStatus<T>>::put(s);
		Ok(())
	}

	fn suspend_channel(target: ParaId) {
		<OutboundXcmpStatus<T>>::mutate(|s| {
			if let Some(details) = s.iter_mut().find(|item| item.recipient == target) {
				let ok = details.state == OutboundState::Ok;
				defensive_assert!(ok, "WARNING: Attempt to suspend channel that was not Ok.");
				details.state = OutboundState::Suspended;
			} else {
				if s.try_push(OutboundChannelDetails::new(target).with_suspended_state()).is_err() {
					defensive!("Cannot pause channel; too many outbound channels");
				}
			}
		});
	}

	fn resume_channel(target: ParaId) {
		<OutboundXcmpStatus<T>>::mutate(|s| {
			if let Some(index) = s.iter().position(|item| item.recipient == target) {
				let suspended = s[index].state == OutboundState::Suspended;
				defensive_assert!(
					suspended,
					"WARNING: Attempt to resume channel that was not suspended."
				);
				if s[index].first_index == s[index].last_index {
					s.remove(index);
				} else {
					s[index].state = OutboundState::Ok;
				}
			} else {
				defensive!("WARNING: Attempt to resume channel that was not suspended.");
			}
		});
	}

	fn enqueue_xcmp_messages<'a>(
		sender: ParaId,
		xcms: &[BoundedSlice<'a, u8, MaxXcmpMessageLenOf<T>>],
		is_first_sender_batch: bool,
		meter: &mut WeightMeter,
	) -> Result<(), ()> {
		let QueueConfigData { drop_threshold, .. } = <QueueConfig<T>>::get();
		let batches_footprints =
			T::XcmpQueue::get_batches_footprints(sender, xcms.iter().copied(), drop_threshold);

		let best_batch_footprint = batches_footprints.search_best_by(|batch_info| {
			let required_weight = T::WeightInfo::enqueue_xcmp_messages(
				batches_footprints.first_page_pos.saturated_into(),
				batch_info,
				is_first_sender_batch,
			);

			match meter.can_consume(required_weight) {
				true => core::cmp::Ordering::Less,
				false => core::cmp::Ordering::Greater,
			}
		});

		meter.consume(T::WeightInfo::enqueue_xcmp_messages(
			batches_footprints.first_page_pos.saturated_into(),
			best_batch_footprint,
			is_first_sender_batch,
		));
		T::XcmpQueue::enqueue_messages(
			xcms.iter().take(best_batch_footprint.msgs_count).copied(),
			sender,
		);

		if best_batch_footprint.msgs_count < xcms.len() {
			tracing::error!(
				target: LOG_TARGET,
				used_weight=?meter.consumed_ratio(),
				"Out of weight: cannot enqueue entire XCMP messages batch; \
				dropped some or all messages in batch."
			);
			return Err(());
		}
		Ok(())
	}

	/// Split concatenated encoded `VersionedXcm`s into individual items.
	///
	/// We directly encode them again since that is needed later on.
	///
	/// On error returns a partial batch with all the XCMs processed before the failure.
	/// This can happen in case of a decoding/re-encoding failure.
	pub(crate) fn take_first_concatenated_xcm<'a>(
		data: &mut &'a [u8],
		meter: &mut WeightMeter,
	) -> Result<BoundedSlice<'a, u8, MaxXcmpMessageLenOf<T>>, ()> {
		// Let's make sure that we can decode at least an empty xcm message.
		let base_weight = T::WeightInfo::take_first_concatenated_xcm(0);
		if meter.try_consume(base_weight).is_err() {
			defensive!("Out of weight; could not decode all; dropping");
			return Err(());
		}

		let input_data = &mut &data[..];
		let mut input = codec::CountedInput::new(input_data);
		VersionedXcm::<()>::decode_with_depth_limit(MAX_XCM_DECODE_DEPTH, &mut input).map_err(
			|error| {
				tracing::debug!(target: LOG_TARGET, ?error, "Failed to decode XCM with depth limit");
				()
			},
		)?;
		let (xcm_data, remaining_data) = data.split_at(input.count() as usize);
		*data = remaining_data;

		// Consume the extra weight that it took to decode this message.
		// This depends on the message len in bytes.
		// Saturates if it's over the limit.
		let extra_weight =
			T::WeightInfo::take_first_concatenated_xcm(xcm_data.len() as u32) - base_weight;
		meter.consume(extra_weight);

		let xcm = BoundedSlice::try_from(xcm_data).map_err(|error| {
			tracing::error!(
				target: LOG_TARGET,
				?error,
				"Failed to take XCM after decoding: message is too long"
			);
			()
		})?;

		Ok(xcm)
	}

	/// Split concatenated opaque `VersionedXcm`s into individual items.
	///
	/// This method is not benchmarked because it's almost free.
	pub(crate) fn take_first_concatenated_opaque_xcm<'a>(
		data: &mut &'a [u8],
	) -> Result<BoundedSlice<'a, u8, MaxXcmpMessageLenOf<T>>, ()> {
		let xcm_len = Compact::<u32>::decode(data).map_err(|error| {
			tracing::debug!(target: LOG_TARGET, ?error, "Failed to decode opaque XCM length");
			()
		})?;
		let (xcm_data, remaining_data) = match data.split_at_checked(xcm_len.0 as usize) {
			Some((xcm_data, remaining_data)) => (xcm_data, remaining_data),
			None => {
				tracing::debug!(target: LOG_TARGET, ?xcm_len, "Wrong opaque XCM length");
				return Err(());
			},
		};
		*data = remaining_data;

		let xcm = BoundedSlice::try_from(xcm_data).map_err(|error| {
			tracing::error!(
				target: LOG_TARGET,
				?error,
				"Failed to take opaque XCM after decoding: message is too long"
			);
			()
		})?;

		Ok(xcm)
	}

	/// Split concatenated encoded `VersionedXcm`s into batches.
	///
	/// We directly encode them again since that is needed later on.
	pub(crate) fn take_first_concatenated_xcms<'a>(
		data: &mut &'a [u8],
		encoding: XcmEncoding,
		batch_size: usize,
		meter: &mut WeightMeter,
	) -> Result<
		Vec<BoundedSlice<'a, u8, MaxXcmpMessageLenOf<T>>>,
		Vec<BoundedSlice<'a, u8, MaxXcmpMessageLenOf<T>>>,
	> {
		let mut batch = vec![];
		loop {
			if data.is_empty() {
				return Ok(batch);
			}

			let maybe_xcm = match encoding {
				XcmEncoding::Simple => Self::take_first_concatenated_xcm(data, meter),
				XcmEncoding::Double => Self::take_first_concatenated_opaque_xcm(data),
			};
			match maybe_xcm {
				Ok(xcm) => {
					batch.push(xcm);
					if batch.len() >= batch_size {
						return Ok(batch);
					}
				},
				Err(_) => return Err(batch),
			}
		}
	}

	/// The worst-case weight of `on_idle`.
	pub fn on_idle_weight() -> Weight {
		<T as crate::Config>::WeightInfo::on_idle_good_msg()
			.max(<T as crate::Config>::WeightInfo::on_idle_large_msg())
	}

	#[cfg(feature = "bridging")]
	fn is_inbound_channel_suspended(sender: ParaId) -> bool {
		<InboundXcmpSuspended<T>>::get().iter().any(|c| c == &sender)
	}

	#[cfg(feature = "bridging")]
	/// Returns tuple of `OutboundState` and number of queued pages.
	fn outbound_channel_state(target: ParaId) -> Option<(OutboundState, u16)> {
		<OutboundXcmpStatus<T>>::get().iter().find(|c| c.recipient == target).map(|c| {
			let queued_pages = c.last_index.saturating_sub(c.first_index);
			(c.state, queued_pages)
		})
	}
}

impl<T: Config> OnQueueChanged<ParaId> for Pallet<T> {
	// Suspends/Resumes the queue when certain thresholds are reached.
	fn on_queue_changed(para: ParaId, fp: QueueFootprint) {
		let QueueConfigData { resume_threshold, suspend_threshold, .. } = <QueueConfig<T>>::get();

		let mut suspended_channels = <InboundXcmpSuspended<T>>::get();
		let suspended = suspended_channels.contains(&para);

		if suspended && fp.ready_pages <= resume_threshold {
			if let Err(err) = Self::send_signal(para, ChannelSignal::Resume) {
				tracing::error!(
					target: LOG_TARGET,
					error=?err,
					sibling=?para,
					"defensive: Could not send resumption signal to inbound channel of sibling; channel remains suspended."
				);
			} else {
				suspended_channels.remove(&para);
				<InboundXcmpSuspended<T>>::put(suspended_channels);
			}
		} else if !suspended && fp.ready_pages >= suspend_threshold {
			tracing::warn!(target: LOG_TARGET, sibling=?para, "XCMP queue for sibling is full; suspending channel.");

			if let Err(err) = Self::send_signal(para, ChannelSignal::Suspend) {
				// It will retry if `drop_threshold` is not reached, but it could be too late.
				tracing::error!(
					target: LOG_TARGET, error=?err,
					"defensive: Could not send suspension signal; future messages may be dropped."
				);
			} else if let Err(err) = suspended_channels.try_insert(para) {
				tracing::error!(
					target: LOG_TARGET,
					error=?err,
					sibling=?para,
					"Too many channels suspended; cannot suspend sibling; further messages may be dropped."
				);
			} else {
				<InboundXcmpSuspended<T>>::put(suspended_channels);
			}
		}
	}
}

impl<T: Config> QueuePausedQuery<ParaId> for Pallet<T> {
	fn is_paused(para: &ParaId) -> bool {
		if !QueueSuspended::<T>::get() {
			return false;
		}

		// Make an exception for the superuser queue:
		let sender_origin = T::ControllerOriginConverter::convert_origin(
			(Parent, Parachain((*para).into())),
			OriginKind::Superuser,
		);
		let is_controller =
			sender_origin.map_or(false, |origin| T::ControllerOrigin::try_origin(origin).is_ok());

		!is_controller
	}
}

/// The encoding of the XCM messages in an XCMP page.
#[derive(Copy, Clone)]
enum XcmEncoding {
	/// Simple encoded (`xcm.encode()`)
	///
	/// When we receive this king of messages, we have to decode and then re-encoded them before
	/// enqueueing them for later processing.
	Simple,
	/// Double encoded (`xcm.encode().encode()`)
	///
	/// The XCM message is encoded first, resulting a vector of bytes. And then the vector of bytes
	/// is encoded again. This has 2 advantages:
	/// 1. We can just decode them before enqueueing them for later processing. They don't need to
	///    be re-encoded.
	/// 2. Decoding a `Vec<u8>` is much more efficient than decoding XCM messages.
	Double,
}

impl<T: Config> XcmpMessageHandler for Pallet<T> {
	fn handle_xcmp_messages<'a, I: Iterator<Item = (ParaId, RelayBlockNumber, &'a [u8])>>(
		iter: I,
		max_weight: Weight,
	) -> Weight {
		let mut meter = WeightMeter::with_limit(max_weight);

		let mut known_xcm_senders = BTreeSet::new();
		for (sender, _sent_at, mut data) in iter {
			let format = match XcmpMessageFormat::decode(&mut data) {
				Ok(f) => f,
				Err(_) => {
					defensive!("Unknown XCMP message format - dropping");
					continue;
				},
			};

			match format {
				XcmpMessageFormat::Signals => {
					let mut signal_count = 0;
					while !data.is_empty() && signal_count < MAX_SIGNALS_PER_PAGE {
						signal_count += 1;
						match ChannelSignal::decode(&mut data) {
							Ok(ChannelSignal::Suspend) => {
								if meter.try_consume(T::WeightInfo::suspend_channel()).is_err() {
									defensive!(
										"Not enough weight to process suspend signal - dropping"
									);
									break;
								}
								Self::suspend_channel(sender)
							},
							Ok(ChannelSignal::Resume) => {
								if meter.try_consume(T::WeightInfo::resume_channel()).is_err() {
									defensive!(
										"Not enough weight to process resume signal - dropping"
									);
									break;
								}
								Self::resume_channel(sender)
							},
							Err(_) => {
								defensive!("Undecodable channel signal - dropping");
								break;
							},
						}
					}
				},
				XcmpMessageFormat::ConcatenatedVersionedXcm |
				XcmpMessageFormat::ConcatenatedOpaqueVersionedXcm => {
					let encoding = match format {
						XcmpMessageFormat::ConcatenatedVersionedXcm => XcmEncoding::Simple,
						XcmpMessageFormat::ConcatenatedOpaqueVersionedXcm => {
							let mut all_channels = <OutboundXcmpStatus<T>>::get();
							if let Some(channel_details) =
								Self::try_get_or_insert_outbound_channel(&mut all_channels, sender)
							{
								channel_details
									.flags
									.notice_concatenated_opaque_versioned_xcm_support();
							}
							<OutboundXcmpStatus<T>>::put(all_channels);

							XcmEncoding::Double
						},
						_ => {
							// This branch is unreachable.
							continue;
						},
					};

					let mut is_first_sender_batch = known_xcm_senders.insert(sender);
					if is_first_sender_batch {
						if meter
							.try_consume(T::WeightInfo::uncached_enqueue_xcmp_messages())
							.is_err()
						{
							defensive!(
								"Out of weight: cannot enqueue XCMP messages; dropping page; \
                                    Used weight: ",
								meter.consumed_ratio()
							);
							continue;
						}
					}

					let mut can_process_next_batch = true;
					while can_process_next_batch {
						let batch = match Self::take_first_concatenated_xcms(
							&mut data,
							encoding,
							XCM_BATCH_SIZE,
							&mut meter,
						) {
							Ok(batch) => batch,
							Err(batch) => {
								can_process_next_batch = false;
								defensive!(
									"HRMP inbound decode stream broke; page will be dropped."
								);
								batch
							},
						};
						if batch.is_empty() {
							break;
						}

						if let Err(()) = Self::enqueue_xcmp_messages(
							sender,
							&batch,
							is_first_sender_batch,
							&mut meter,
						) {
							break;
						}
						is_first_sender_batch = false;
					}
				},
				XcmpMessageFormat::ConcatenatedEncodedBlob => {
					defensive!("Blob messages are unhandled - dropping");
					continue;
				},
			}
		}

		meter.consumed()
	}
}

impl<T: Config> XcmpMessageSource for Pallet<T> {
	fn take_outbound_messages(
		maximum_channels: usize,
		excluded_recipients: &[ParaId],
	) -> Vec<(ParaId, Vec<u8>)> {
		let mut statuses = <OutboundXcmpStatus<T>>::get().into_inner();
		let old_statuses_len = statuses.len();
		let max_message_count = statuses.len().min(maximum_channels);
		let mut result = Vec::with_capacity(max_message_count);

		statuses.retain_mut(|status| {
			let OutboundChannelDetails {
				recipient: para_id,
				state: outbound_state,
				signals_exist,
				first_index,
				last_index,
				flags,
				queued_bytes,
			} = status;

			let (max_size_now, max_size_ever) = match T::ChannelInfo::get_channel_status(*para_id) {
				ChannelStatus::Closed => {
					// This means that there is no such channel anymore. Nothing to be done but
					// swallow the messages and discard the status.
					for i in *first_index..*last_index {
						<OutboundXcmpMessages<T>>::remove(*para_id, i);
					}
					if *signals_exist {
						<SignalMessages<T>>::remove(*para_id);
					}
					return false;
				},
				ChannelStatus::Full => return true,
				ChannelStatus::Ready(max_size_now, max_size_ever) => (max_size_now, max_size_ever),
			};

			// Check if we should omit the recipient.
			if excluded_recipients.contains(para_id) {
				return true;
			}

			// This is a hard limit from the host config; not even signals can bypass it.
			if result.len() == max_message_count {
				// We check this condition in the beginning of the loop so that we don't include
				// a message where the limit is 0.
				return true;
			}

			let page = 'page_fetch: {
				if *signals_exist {
					let page = <SignalMessages<T>>::get(*para_id);
					defensive_assert!(!page.is_empty(), "Signals must exist");

					if page.len() < max_size_now {
						<SignalMessages<T>>::remove(*para_id);
						*signals_exist = false;
						break 'page_fetch page;
					}

					defensive!("Signals should fit into a single page");
					return true;
				}

				if *outbound_state == OutboundState::Suspended {
					// Only signals are exempt from suspension.
					return true;
				}

				if last_index > first_index {
					let page = <OutboundXcmpMessages<T>>::get(*para_id, *first_index);
					if page.len() < max_size_now {
						<OutboundXcmpMessages<T>>::remove(*para_id, *first_index);
						*first_index += 1;
						*queued_bytes = queued_bytes.saturating_sub(page.len() as u32);
						break 'page_fetch page;
					}
				}

				// Send a notification to the recipient advertising that we support
				// `XcmpMessageFormat::ConcatenatedOpaqueVersionedXcm` if needed.
				// We do this only once during the entire lifetime of the channel.
				if flags.should_send_concatenated_opaque_versioned_xcm_notification() {
					match WeakBoundedVec::try_from(XcmpMessageFormat::ConcatenatedOpaqueVersionedXcm.encode()) {
						Ok(page) => {
							flags.notice_concatenated_opaque_versioned_xcm_notification_sent();
							break 'page_fetch page;
						},
						Err(_) => {
							defensive!("XcmpMessageFormat should fit into a single page");
							return true;
						}
					};
				}

				return true;
			};

			if first_index == last_index {
				*first_index = 0;
				*last_index = 0;
				*queued_bytes = 0;
			}

			if page.len() > max_size_ever {
				// TODO: #274 This means that the channel's max message size has changed since
				//   the message was sent. We should parse it and split into smaller messages but
				//   since it's so unlikely then for now we just drop it.
				defensive!("WARNING: oversize message in queue - dropping");
			} else {
				result.push((*para_id, page.into_inner()));
			}

			let max_total_size = match T::ChannelInfo::get_channel_info(*para_id) {
				Some(channel_info) => channel_info.max_total_size,
				None => {
					tracing::warn!(target: LOG_TARGET, "calling `get_channel_info` with no RelevantMessagingState?!");
					// We use this as a fallback in case the messaging state is not present
					MAX_POSSIBLE_ALLOCATION
				},
			};
			let threshold = max_total_size.saturating_div(delivery_fee_constants::THRESHOLD_FACTOR);
			if *queued_bytes <= threshold {
				Self::decrease_fee_factor(*para_id);
			}

			true
		});
		debug_assert!(!statuses.iter().any(|s| s.signals_exist), "Signals should be handled");
		let mut statuses = BoundedVec::defensive_truncate_from(statuses);

		// Sort the outbound messages by ascending recipient para id to satisfy the acceptance
		// criteria requirement.
		result.sort_by_key(|(recipient, _msg)| *recipient);

		// old_status_len must be >= status.len() since we never add anything to status.
		let pruned = old_statuses_len - statuses.len();
		// Because it may so happen that we only gave attention to some channels it's important
		// to change the order. Otherwise, the next `on_finalize` we will again give attention
		// only to those channels that happen to be in the beginning, until they are emptied.
		// This leads to "starvation" of the channels near to the end.
		let _ = statuses.try_rotate_left(result.len().saturating_sub(pruned)).defensive_proof(
			"Could not store HRMP channels config. Some HRMP channels may be broken.",
		);

		<OutboundXcmpStatus<T>>::put(statuses);

		result
	}
}

/// Xcm sender for sending to a sibling parachain.
impl<T: Config> SendXcm for Pallet<T> {
	type Ticket = (ParaId, VersionedXcm<()>);

	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<(ParaId, VersionedXcm<()>)> {
		let d = dest.take().ok_or(SendError::MissingArgument)?;

		match d.unpack() {
			// An HRMP message for a sibling parachain.
			(1, [Parachain(id)]) => {
				let xcm = msg.take().ok_or(SendError::MissingArgument)?;
				let id = ParaId::from(*id);
				let price = T::PriceForSiblingDelivery::price_for_delivery(id, &xcm);
				let versioned_xcm = T::VersionWrapper::wrap_version(&d, xcm)
					.map_err(|()| SendError::DestinationUnsupported)?;
				versioned_xcm
					.check_is_decodable()
					.map_err(|()| SendError::ExceedsMaxMessageSize)?;

				Ok(((id, versioned_xcm), price))
			},
			_ => {
				// Anything else is unhandled. This includes a message that is not meant for us.
				// We need to make sure that dest/msg is not consumed here.
				*dest = Some(d);
				Err(SendError::NotApplicable)
			},
		}
	}

	fn deliver((recipient, xcm): (ParaId, VersionedXcm<()>)) -> Result<XcmHash, SendError> {
		let hash = xcm.using_encoded(sp_io::hashing::blake2_256);

		let mut encoding = XcmEncoding::Simple;
		let mut all_channels = <OutboundXcmpStatus<T>>::get();
		if let Some(channel_details) = Self::try_get_outbound_channel(&mut all_channels, recipient)
		{
			if channel_details.flags.has_concatenated_opaque_versioned_xcm_support() {
				encoding = XcmEncoding::Double;
			}
		}

		let result = match encoding {
			XcmEncoding::Simple => {
				Self::send_fragment(recipient, XcmpMessageFormat::ConcatenatedVersionedXcm, xcm)
			},
			XcmEncoding::Double => Self::send_fragment(
				recipient,
				XcmpMessageFormat::ConcatenatedOpaqueVersionedXcm,
				xcm.encode(),
			),
		};
		match result {
			Ok(_) => {
				Self::deposit_event(Event::XcmpMessageSent { message_hash: hash });
				Ok(hash)
			},
			Err(e) => {
				tracing::error!(target: LOG_TARGET, error=?e, "Deliver error");
				Err(SendError::Transport(e.into()))
			},
		}
	}
}

impl<T: Config> InspectMessageQueues for Pallet<T> {
	fn clear_messages() {
		// Best effort.
		let _ = OutboundXcmpMessages::<T>::clear(u32::MAX, None);
		OutboundXcmpStatus::<T>::mutate(|details_vec| {
			for details in details_vec {
				details.first_index = 0;
				details.last_index = 0;
				details.queued_bytes = 0;
			}
		});
	}

	fn get_messages() -> Vec<(VersionedLocation, Vec<VersionedXcm<()>>)> {
		use xcm::prelude::*;

		OutboundXcmpMessages::<T>::iter()
			.map(|(para_id, _, messages)| {
				let data = &mut &messages[..];

				let decoded_format = XcmpMessageFormat::decode(data).unwrap();
				let mut decoded_messages = Vec::new();
				while !data.is_empty() {
					let message_bytes = match decoded_format {
						XcmpMessageFormat::ConcatenatedVersionedXcm => {
							Self::take_first_concatenated_xcm(data, &mut WeightMeter::new())
						},
						XcmpMessageFormat::ConcatenatedOpaqueVersionedXcm => {
							Self::take_first_concatenated_opaque_xcm(data)
						},
						unexpected_format => {
							panic!("Unexpected XCMP format: {unexpected_format:?}!")
						},
					}
					.unwrap();
					let decoded_message = VersionedXcm::<()>::decode_all_with_mem_and_depth_limit(
						&mut &message_bytes[..],
					)
					.unwrap();
					decoded_messages.push(decoded_message);
				}

				(
					VersionedLocation::from(Location::new(1, Parachain(para_id.into()))),
					decoded_messages,
				)
			})
			.collect()
	}
}

impl<T: Config> FeeTracker for Pallet<T> {
	type Id = ParaId;

	fn get_fee_factor(id: Self::Id) -> FixedU128 {
		<DeliveryFeeFactor<T>>::get(id)
	}

	fn set_fee_factor(id: Self::Id, val: FixedU128) {
		<DeliveryFeeFactor<T>>::set(id, val);
	}
}

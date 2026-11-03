// Copyright (C) Parity Technologies (UK) Ltd.
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

//! Pallet that may be used instead of `SovereignPaidRemoteExporter` or `UnpaidRemoteExporter`
//! in the XCM router configuration. The main thing that the pallet offers is the dynamic
//! message fee, that is computed based on the bridge queues state. It starts exponentially
//! increasing if the queue between this chain and the sibling/child bridge hub is congested.
//!
//! The pallet is configurable to use either paid or unpaid execution on the bridge hub
//! via the [`Config::UnpaidExport`] associated type. It will use `SovereignPaidRemoteExporter`
//! for sovereign-paid bridging or `UnpaidRemoteExporter` for unpaid bridging (e.g. between
//! system parachains where the bridge hub waives fees).
//!
//! All other bridge hub queues offer some backpressure mechanisms. So if at least one
//! of all queues is congested, it will eventually lead to the growth of the queue at
//! this chain.
//!
//! **A note on terminology**: when we mention the bridge hub here, we mean the chain that
//! has the messages pallet deployed (`pallet-bridge-grandpa`, `pallet-bridge-messages`,
//! `pallet-xcm-bridge-hub`, ...). It may be the system bridge hub parachain or any other
//! chain.

#![cfg_attr(not(feature = "std"), no_std)]

use bp_xcm_bridge_hub_router::MINIMAL_DELIVERY_FEE_FACTOR;
pub use bp_xcm_bridge_hub_router::{BridgeState, XcmChannelStatusProvider};
use codec::Encode;
use frame_support::traits::Get;
use polkadot_runtime_parachains::FeeTracker;
use sp_core::H256;
use sp_runtime::{FixedPointNumber, FixedU128};
use sp_std::vec::Vec;
use xcm::prelude::*;
use xcm_builder::{
	ExporterFor, InspectMessageQueues, SovereignPaidRemoteExporter, UnpaidRemoteExporter,
};

pub use pallet::*;
pub use weights::WeightInfo;

pub mod benchmarking;
pub mod weights;

mod mock;

/// Maximal size of the XCM message that may be sent over bridge.
///
/// This should be less than the maximal size, allowed by the messages pallet, because
/// the message itself is wrapped in other structs and is double encoded.
pub const HARD_MESSAGE_SIZE_LIMIT: u32 = 32 * 1024;

/// The target that will be used when publishing logs related to this pallet.
///
/// This doesn't match the pattern used by other bridge pallets (`runtime::bridge-*`). But this
/// pallet has significant differences with those pallets. The main one is that is intended to
/// be deployed at sending chains. Other bridge pallets are likely to be deployed at the separate
/// bridge hub parachain.
pub const LOG_TARGET: &str = "xcm::bridge-hub-router";

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self, I>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;
		/// Benchmarks results from runtime we're plugged into.
		type WeightInfo: WeightInfo;

		/// Universal location of this runtime.
		type UniversalLocation: Get<InteriorLocation>;
		/// Relative location of the supported sibling bridge hub.
		type SiblingBridgeHubLocation: Get<Location>;
		/// The bridged network that this config is for if specified.
		/// Also used for filtering `Bridges` by `BridgedNetworkId`.
		/// If not specified, allows all networks pass through.
		type BridgedNetworkId: Get<Option<NetworkId>>;
		/// Configuration for supported **bridged networks/locations** with **bridge location** and
		/// **possible fee**. Allows to externalize better control over allowed **bridged
		/// networks/locations**.
		type Bridges: ExporterFor;
		/// Checks the XCM version for the destination.
		type DestinationVersion: GetVersion;

		/// Origin of the sibling bridge hub that is allowed to report bridge status.
		type BridgeHubOrigin: EnsureOrigin<Self::RuntimeOrigin>;
		/// Actual message sender (`HRMP` or `DMP`) to the sibling bridge hub location.
		type ToBridgeHubSender: SendXcm;
		/// Local XCM channel manager.
		type LocalXcmChannelManager: XcmChannelStatusProvider;

		/// Whether to use unpaid execution when sending export messages to the bridge hub.
		/// Set to `ConstBool<true>` for system parachains where the bridge hub waives fees,
		/// `ConstBool<false>` for sovereign-paid bridging.
		type UnpaidExport: Get<bool>;

		/// Additional fee that is paid for every byte of the outbound message.
		type ByteFee: Get<u128>;
		/// Asset that is used to paid bridge fee.
		type FeeAsset: Get<AssetId>;
	}

	#[pallet::pallet]
	pub struct Pallet<T, I = ()>(PhantomData<(T, I)>);

	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<BlockNumberFor<T>> for Pallet<T, I> {
		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			// if XCM channel is still congested, we don't change anything
			if T::LocalXcmChannelManager::is_congested(&T::SiblingBridgeHubLocation::get()) {
				return T::WeightInfo::on_initialize_when_congested();
			}

			// if bridge has reported congestion, we don't change anything
			let mut bridge = Self::bridge();
			if bridge.is_congested {
				return T::WeightInfo::on_initialize_when_congested();
			}

			let previous_factor = Self::get_fee_factor(());
			// if we can't decrease the delivery fee factor anymore, we don't change anything
			if !Self::do_decrease_fee_factor(&mut bridge.delivery_fee_factor) {
				return T::WeightInfo::on_initialize_when_congested();
			}

			tracing::info!(
				target: LOG_TARGET,
				from=%previous_factor,
				to=%bridge.delivery_fee_factor,
				"Bridge channel is uncongested. Decreased fee factor"
			);
			Self::deposit_event(Event::DeliveryFeeFactorDecreased {
				new_value: bridge.delivery_fee_factor,
			});

			Bridge::<T, I>::put(bridge);

			T::WeightInfo::on_initialize_when_non_congested()
		}
	}

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Notification about congested bridge queue.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::report_bridge_status())]
		pub fn report_bridge_status(
			origin: OriginFor<T>,
			// this argument is not currently used, but to ease future migration, we'll keep it
			// here
			bridge_id: H256,
			is_congested: bool,
		) -> DispatchResult {
			T::BridgeHubOrigin::ensure_origin(origin)?;

			tracing::info!(
				target: LOG_TARGET,
				from=?bridge_id,
				congested=%is_congested,
				"Received bridge status"
			);

			Bridge::<T, I>::mutate(|bridge| {
				bridge.is_congested = is_congested;
			});
			Ok(())
		}
	}

	/// Bridge that we are using.
	///
	/// **bridges-v1** assumptions: all outbound messages through this router are using single lane
	/// and to single remote consensus. If there is some other remote consensus that uses the same
	/// bridge hub, the separate pallet instance shall be used, In `v2` we'll have all required
	/// primitives (lane-id aka bridge-id, derived from XCM locations) to support multiple  bridges
	/// by the same pallet instance.
	#[pallet::storage]
	pub type Bridge<T: Config<I>, I: 'static = ()> = StorageValue<_, BridgeState, ValueQuery>;

	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Bridge that we are using.
		pub fn bridge() -> BridgeState {
			Bridge::<T, I>::get()
		}

		/// Called when new message is sent (queued to local outbound XCM queue) over the bridge.
		pub(crate) fn on_message_sent_to_bridge(message_size: u32) {
			tracing::trace!(
				target: LOG_TARGET,
				?message_size, "on_message_sent_to_bridge"
			);
			let _ = Bridge::<T, I>::try_mutate(|bridge| {
				let is_channel_with_bridge_hub_congested =
					T::LocalXcmChannelManager::is_congested(&T::SiblingBridgeHubLocation::get());
				let is_bridge_congested = bridge.is_congested;

				// if outbound queue is not congested AND bridge has not reported congestion, do
				// nothing
				if !is_channel_with_bridge_hub_congested && !is_bridge_congested {
					return Err(());
				}

				let previous_factor = Self::get_fee_factor(());
				// ok - we need to increase the fee factor, let's do that
				<Self as FeeTracker>::do_increase_fee_factor(
					&mut bridge.delivery_fee_factor,
					message_size as u128,
				);

				tracing::info!(
					target: LOG_TARGET,
					from=%previous_factor,
					to=%bridge.delivery_fee_factor,
					"Bridge channel is congested. Increased fee factor"
				);
				Self::deposit_event(Event::DeliveryFeeFactorIncreased {
					new_value: bridge.delivery_fee_factor,
				});
				Ok(())
			});
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// Delivery fee factor has been decreased.
		DeliveryFeeFactorDecreased {
			/// New value of the `DeliveryFeeFactor`.
			new_value: FixedU128,
		},
		/// Delivery fee factor has been increased.
		DeliveryFeeFactorIncreased {
			/// New value of the `DeliveryFeeFactor`.
			new_value: FixedU128,
		},
	}
}

// This pallet acts as the `ExporterFor` for the inner exporter to compute
// message fee using fee factor.
impl<T: Config<I>, I: 'static> ExporterFor for Pallet<T, I> {
	fn exporter_for(
		network: &NetworkId,
		remote_location: &InteriorLocation,
		message: &Xcm<()>,
	) -> Option<(Location, Option<Asset>)> {
		tracing::trace!(
			target: LOG_TARGET,
			?network, ?remote_location, msg=?message, "exporter_for"
		);
		// ensure that the message is sent to the expected bridged network (if specified).
		if let Some(bridged_network) = T::BridgedNetworkId::get() {
			if *network != bridged_network {
				tracing::trace!(
					target: LOG_TARGET,
					bridged_network_id=?bridged_network, ?network, "Router does not support bridging!"
				);
				return None;
			}
		}

		// ensure that the message is sent to the expected bridged network and location.
		let (bridge_hub_location, maybe_payment) =
			match T::Bridges::exporter_for(network, remote_location, message) {
				Some((bridge_hub_location, maybe_payment))
					if bridge_hub_location.eq(&T::SiblingBridgeHubLocation::get()) =>
				{
					(bridge_hub_location, maybe_payment)
				},
				_ => {
					tracing::trace!(
						target: LOG_TARGET,
						bridged_network_id=?T::BridgedNetworkId::get(),
						sibling_bridge_hub_location=?T::SiblingBridgeHubLocation::get(),
						?network,
						?remote_location,
						"Router configured does not support bridging!"
					);
					return None;
				},
			};

		// take `base_fee` from `T::Brides`, but it has to be the same `T::FeeAsset`
		let base_fee = match maybe_payment {
			Some(payment) => match payment {
				Asset { fun: Fungible(amount), id } if id.eq(&T::FeeAsset::get()) => amount,
				invalid_asset => {
					tracing::error!(
						target: LOG_TARGET,
						bridged_network_id=?T::BridgedNetworkId::get(),
						fee_asset=?T::FeeAsset::get(),
						with=?invalid_asset,
						?bridge_hub_location,
						?network,
						?remote_location,
						"Router is configured for `T::FeeAsset` which is not compatible for bridging!"
					);
					return None;
				},
			},
			None => 0,
		};

		// compute fee amount. Keep in mind that this is only the bridge fee. The fee for sending
		// message from this chain to child/sibling bridge hub is determined by the
		// `Config::ToBridgeHubSender`
		let message_size = message.encoded_size();
		let message_fee = (message_size as u128).saturating_mul(T::ByteFee::get());
		let fee_sum = base_fee.saturating_add(message_fee);
		let fee_factor = Self::get_fee_factor(());
		let fee = fee_factor.saturating_mul_int(fee_sum);

		let fee = if fee > 0 { Some((T::FeeAsset::get(), fee).into()) } else { None };

		tracing::info!(
			target: LOG_TARGET,
			to=?(network, remote_location),
			bridge_fee=?fee,
			%fee_factor,
			"Going to send message ({message_size} bytes) over bridge."
		);

		Some((bridge_hub_location, fee))
	}
}

// This pallet acts as the `SendXcm` to the sibling/child bridge hub instead of regular
// XCMP/DMP transport. This allows injecting dynamic message fees into XCM programs that
// are going to the bridged network.
impl<T: Config<I>, I: 'static> SendXcm for Pallet<T, I> {
	type Ticket = (u32, <T::ToBridgeHubSender as SendXcm>::Ticket);

	fn validate(
		dest: &mut Option<Location>,
		xcm: &mut Option<Xcm<()>>,
	) -> SendResult<Self::Ticket> {
		tracing::trace!(target: LOG_TARGET, msg=?xcm, destination=?dest, "validate");

		// In case of success, the inner exporter can modify XCM instructions and consume
		// `dest` / `xcm`, so we retain the clone of original message and the destination for later
		// `DestinationVersion` validation.
		let xcm_to_dest_clone = xcm.clone();
		let dest_clone = dest.clone();

		// First, use the inner exporter to validate the destination to determine if it is even
		// routable. If it is not, return an error. If it is, then the XCM is extended with
		// instructions to pay the message fee at the sibling/child bridge hub. The cost will
		// include both the cost of (1) delivery to the sibling bridge hub and (2) delivery
		// to the bridged bridge hub (returned by `Self::exporter_for`).
		let exporter_result = if T::UnpaidExport::get() {
			UnpaidRemoteExporter::<
				Pallet<T, I>,
				T::ToBridgeHubSender,
				T::UniversalLocation,
			>::validate(dest, xcm)
		} else {
			SovereignPaidRemoteExporter::<
				Pallet<T, I>,
				T::ToBridgeHubSender,
				T::UniversalLocation,
			>::validate(dest, xcm)
		};
		match exporter_result {
			Ok((ticket, cost)) => {
				// If the ticket is ok, it means we are routing with this router, so we need to
				// apply more validations to the cloned `dest` and `xcm`, which are required here.
				let xcm_to_dest_clone = xcm_to_dest_clone.ok_or(SendError::MissingArgument)?;
				let dest_clone = dest_clone.ok_or(SendError::MissingArgument)?;

				// We won't have access to `dest` and `xcm` in the `deliver` method, so we need to
				// precompute everything required here. However, `dest` and `xcm` were consumed by
				// the inner exporter, so we need to use their clones.
				let message_size = xcm_to_dest_clone.encoded_size() as _;

				// The bridge doesn't support oversized or overweight messages. Therefore, it's
				// better to drop such messages here rather than at the bridge hub. Let's check the
				// message size."
				if message_size > HARD_MESSAGE_SIZE_LIMIT {
					return Err(SendError::ExceedsMaxMessageSize);
				}

				// We need to ensure that the known `dest`'s XCM version can comprehend the current
				// `xcm` program. This may seem like an additional, unnecessary check, but it is
				// not. A similar check is probably performed by the inner exporter, which
				// attempts to send a versioned message to the sibling bridge hub. However, the
				// local bridge hub may have a higher XCM version than the remote `dest`. Once
				// again, it is better to discard such messages here than at the bridge hub (e.g.,
				// to avoid losing funds).
				let destination_version = T::DestinationVersion::get_version_for(&dest_clone)
					.ok_or(SendError::DestinationUnsupported)?;
				VersionedXcm::from(xcm_to_dest_clone)
					.into_version(destination_version)
					.map_err(|()| SendError::DestinationUnsupported)?;

				Ok(((message_size, ticket), cost))
			},
			Err(e) => {
				tracing::trace!(target: LOG_TARGET, error=?e, "validate - inner exporter");
				Err(e)
			},
		}
	}

	fn deliver(ticket: Self::Ticket) -> Result<XcmHash, SendError> {
		// use router to enqueue message to the sibling/child bridge hub. This also should handle
		// payment for passing through this queue.
		let (message_size, ticket) = ticket;
		let xcm_hash = T::ToBridgeHubSender::deliver(ticket)?;

		// increase delivery fee factor if required
		Self::on_message_sent_to_bridge(message_size);

		tracing::trace!(target: LOG_TARGET, ?xcm_hash, "deliver - message sent");
		Ok(xcm_hash)
	}
}

impl<T: Config<I>, I: 'static> InspectMessageQueues for Pallet<T, I> {
	fn clear_messages() {}

	/// This router needs to implement `InspectMessageQueues` but doesn't have to
	/// return any messages, since it just reuses the `XcmpQueue` router.
	fn get_messages() -> Vec<(VersionedLocation, Vec<VersionedXcm<()>>)> {
		Vec::new()
	}
}

impl<T: Config<I>, I: 'static> FeeTracker for Pallet<T, I> {
	type Id = ();

	const MIN_FEE_FACTOR: FixedU128 = MINIMAL_DELIVERY_FEE_FACTOR;

	fn get_fee_factor(_id: Self::Id) -> FixedU128 {
		Self::bridge().delivery_fee_factor
	}

	fn set_fee_factor(_id: Self::Id, val: FixedU128) {
		let mut bridge = Self::bridge();
		bridge.delivery_fee_factor = val;
		Bridge::<T, I>::put(bridge);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_support::assert_ok;
	use mock::*;

	use frame_support::traits::Hooks;
	use frame_system::{EventRecord, Phase};
	use sp_runtime::traits::One;

	fn congested_bridge(delivery_fee_factor: FixedU128) -> BridgeState {
		BridgeState { is_congested: true, delivery_fee_factor }
	}

	fn uncongested_bridge(delivery_fee_factor: FixedU128) -> BridgeState {
		BridgeState { is_congested: false, delivery_fee_factor }
	}

	#[test]
	fn initial_fee_factor_is_one() {
		run_test(|| {
			assert_eq!(
				Bridge::<TestRuntime, ()>::get(),
				uncongested_bridge(Pallet::<TestRuntime, ()>::MIN_FEE_FACTOR),
			);
		})
	}

	#[test]
	fn fee_factor_is_not_decreased_from_on_initialize_when_xcm_channel_is_congested() {
		run_test(|| {
			Bridge::<TestRuntime, ()>::put(uncongested_bridge(FixedU128::from_rational(125, 100)));
			TestLocalXcmChannelManager::make_congested(&SiblingBridgeHubLocation::get());

			// it should not decrease, because queue is congested
			let old_delivery = XcmBridgeHubRouter::bridge();
			XcmBridgeHubRouter::on_initialize(One::one());
			assert_eq!(XcmBridgeHubRouter::bridge(), old_delivery);
			assert_eq!(System::events(), vec![]);
		})
	}

	#[test]
	fn fee_factor_is_not_decreased_from_on_initialize_when_bridge_has_reported_congestion() {
		run_test(|| {
			Bridge::<TestRuntime, ()>::put(congested_bridge(FixedU128::from_rational(125, 100)));

			// it should not decrease, because bridge congested
			let old_bridge = XcmBridgeHubRouter::bridge();
			XcmBridgeHubRouter::on_initialize(One::one());
			assert_eq!(XcmBridgeHubRouter::bridge(), old_bridge);
			assert_eq!(System::events(), vec![]);
		})
	}

	#[test]
	fn fee_factor_is_decreased_from_on_initialize_when_xcm_channel_is_uncongested() {
		run_test(|| {
			let initial_fee_factor = FixedU128::from_rational(125, 100);
			Bridge::<TestRuntime, ()>::put(uncongested_bridge(initial_fee_factor));

			// it should eventually decrease to one
			while XcmBridgeHubRouter::bridge().delivery_fee_factor >
				Pallet::<TestRuntime, ()>::MIN_FEE_FACTOR
			{
				XcmBridgeHubRouter::on_initialize(One::one());
			}

			// verify that it doesn't decrease anymore
			XcmBridgeHubRouter::on_initialize(One::one());
			assert_eq!(
				XcmBridgeHubRouter::bridge(),
				uncongested_bridge(Pallet::<TestRuntime, ()>::MIN_FEE_FACTOR)
			);

			// check emitted event
			let first_system_event = System::events().first().cloned();
			assert_eq!(
				first_system_event,
				Some(EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::XcmBridgeHubRouter(Event::DeliveryFeeFactorDecreased {
						new_value: initial_fee_factor / XcmBridgeHubRouter::EXPONENTIAL_FEE_BASE,
					}),
					topics: vec![],
				})
			);
		})
	}

	#[test]
	fn not_applicable_if_destination_is_within_other_network() {
		run_test(|| {
			// unroutable dest
			let dest = Location::new(2, [GlobalConsensus(ByGenesis([0; 32])), Parachain(1000)]);
			let xcm: Xcm<()> = vec![ClearOrigin].into();

			// check that router does not consume when `NotApplicable`
			let mut xcm_wrapper = Some(xcm.clone());
			assert_eq!(
				XcmBridgeHubRouter::validate(&mut Some(dest.clone()), &mut xcm_wrapper),
				Err(SendError::NotApplicable),
			);
			// XCM is NOT consumed and untouched
			assert_eq!(Some(xcm.clone()), xcm_wrapper);

			// check the full `send_xcm`
			assert_eq!(send_xcm::<XcmBridgeHubRouter>(dest, xcm,), Err(SendError::NotApplicable),);
		});
	}

	#[test]
	fn exceeds_max_message_size_if_size_is_above_hard_limit() {
		run_test(|| {
			// routable dest with XCM version
			let dest =
				Location::new(2, [GlobalConsensus(BridgedNetworkId::get()), Parachain(1000)]);
			// oversized XCM
			let xcm: Xcm<()> = vec![ClearOrigin; HARD_MESSAGE_SIZE_LIMIT as usize].into();

			// dest is routable with the inner router
			assert_ok!(SovereignPaidRemoteExporter::<
				Pallet<TestRuntime, ()>,
				TestToBridgeHubSender,
				UniversalLocation,
			>::validate(&mut Some(dest.clone()), &mut Some(xcm.clone())));

			// check for oversized message
			let mut xcm_wrapper = Some(xcm.clone());
			assert_eq!(
				XcmBridgeHubRouter::validate(&mut Some(dest.clone()), &mut xcm_wrapper),
				Err(SendError::ExceedsMaxMessageSize),
			);
			// XCM is consumed by the inner router
			assert!(xcm_wrapper.is_none());

			// check the full `send_xcm`
			assert_eq!(
				send_xcm::<XcmBridgeHubRouter>(dest, xcm,),
				Err(SendError::ExceedsMaxMessageSize),
			);
		});
	}

	#[test]
	fn destination_unsupported_if_wrap_version_fails() {
		run_test(|| {
			// routable dest but we don't know XCM version
			let dest = UnknownXcmVersionForRoutableLocation::get();
			let xcm: Xcm<()> = vec![ClearOrigin].into();

			// dest is routable with the inner router
			assert_ok!(SovereignPaidRemoteExporter::<
				Pallet<TestRuntime, ()>,
				TestToBridgeHubSender,
				UniversalLocation,
			>::validate(&mut Some(dest.clone()), &mut Some(xcm.clone())));

			// check that it does not pass XCM version check
			let mut xcm_wrapper = Some(xcm.clone());
			assert_eq!(
				XcmBridgeHubRouter::validate(&mut Some(dest.clone()), &mut xcm_wrapper),
				Err(SendError::DestinationUnsupported),
			);
			// XCM is consumed by the inner router
			assert!(xcm_wrapper.is_none());

			// check the full `send_xcm`
			assert_eq!(
				send_xcm::<XcmBridgeHubRouter>(dest, xcm,),
				Err(SendError::DestinationUnsupported),
			);
		});
	}

	#[test]
	fn returns_proper_delivery_price() {
		run_test(|| {
			let dest = Location::new(2, [GlobalConsensus(BridgedNetworkId::get())]);
			let xcm: Xcm<()> = vec![ClearOrigin].into();
			let msg_size = xcm.encoded_size();

			// initially the base fee is used: `BASE_FEE + BYTE_FEE * msg_size + HRMP_FEE`
			let expected_fee = BASE_FEE + BYTE_FEE * (msg_size as u128) + HRMP_FEE;
			assert_eq!(
				XcmBridgeHubRouter::validate(&mut Some(dest.clone()), &mut Some(xcm.clone()))
					.unwrap()
					.1
					.get(0),
				Some(&(BridgeFeeAsset::get(), expected_fee).into()),
			);

			// but when factor is larger than one, it increases the fee, so it becomes:
			// `(BASE_FEE + BYTE_FEE * msg_size) * F + HRMP_FEE`
			let factor = FixedU128::from_rational(125, 100);
			Bridge::<TestRuntime, ()>::put(uncongested_bridge(factor));
			let expected_fee =
				(FixedU128::saturating_from_integer(BASE_FEE + BYTE_FEE * (msg_size as u128)) *
					factor)
					.into_inner() / FixedU128::DIV +
					HRMP_FEE;
			assert_eq!(
				XcmBridgeHubRouter::validate(&mut Some(dest), &mut Some(xcm)).unwrap().1.get(0),
				Some(&(BridgeFeeAsset::get(), expected_fee).into()),
			);
		});
	}

	#[test]
	fn sent_message_doesnt_increase_factor_if_queue_is_uncongested() {
		run_test(|| {
			let old_bridge = XcmBridgeHubRouter::bridge();
			assert_eq!(
				send_xcm::<XcmBridgeHubRouter>(
					Location::new(2, [GlobalConsensus(BridgedNetworkId::get()), Parachain(1000)]),
					vec![ClearOrigin].into(),
				)
				.map(drop),
				Ok(()),
			);

			assert!(TestToBridgeHubSender::is_message_sent());
			assert_eq!(old_bridge, XcmBridgeHubRouter::bridge());

			assert_eq!(System::events(), vec![]);
		});
	}

	#[test]
	fn sent_message_increases_factor_if_xcm_channel_is_congested() {
		run_test(|| {
			TestLocalXcmChannelManager::make_congested(&SiblingBridgeHubLocation::get());

			let old_bridge = XcmBridgeHubRouter::bridge();
			assert_ok!(send_xcm::<XcmBridgeHubRouter>(
				Location::new(2, [GlobalConsensus(BridgedNetworkId::get()), Parachain(1000)]),
				vec![ClearOrigin].into(),
			)
			.map(drop));

			assert!(TestToBridgeHubSender::is_message_sent());
			assert!(
				old_bridge.delivery_fee_factor < XcmBridgeHubRouter::bridge().delivery_fee_factor
			);

			// check emitted event
			let first_system_event = System::events().first().cloned();
			assert!(matches!(
				first_system_event,
				Some(EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::XcmBridgeHubRouter(
						Event::DeliveryFeeFactorIncreased { .. }
					),
					..
				})
			));
		});
	}

	#[test]
	fn sent_message_increases_factor_if_bridge_has_reported_congestion() {
		run_test(|| {
			Bridge::<TestRuntime, ()>::put(congested_bridge(
				Pallet::<TestRuntime, ()>::MIN_FEE_FACTOR,
			));

			let old_bridge = XcmBridgeHubRouter::bridge();
			assert_ok!(send_xcm::<XcmBridgeHubRouter>(
				Location::new(2, [GlobalConsensus(BridgedNetworkId::get()), Parachain(1000)]),
				vec![ClearOrigin].into(),
			)
			.map(drop));

			assert!(TestToBridgeHubSender::is_message_sent());
			assert!(
				old_bridge.delivery_fee_factor < XcmBridgeHubRouter::bridge().delivery_fee_factor
			);

			// check emitted event
			let first_system_event = System::events().first().cloned();
			assert!(matches!(
				first_system_event,
				Some(EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::XcmBridgeHubRouter(
						Event::DeliveryFeeFactorIncreased { .. }
					),
					..
				})
			));
		});
	}

	#[test]
	fn get_messages_does_not_return_anything() {
		run_test(|| {
			assert_ok!(send_xcm::<XcmBridgeHubRouter>(
				(Parent, Parent, GlobalConsensus(BridgedNetworkId::get()), Parachain(1000)).into(),
				vec![ClearOrigin].into()
			));
			assert_eq!(XcmBridgeHubRouter::get_messages(), vec![]);
		});
	}
}

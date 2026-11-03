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

//! Traits and utilities to help with origin mutation and bridging.

#![allow(deprecated)]

use crate::InspectMessageQueues;
use alloc::{vec, vec::Vec};
use codec::{Decode, Encode};
use core::{convert::TryInto, marker::PhantomData};
use frame_support::{ensure, traits::Get};
use xcm::prelude::*;
use xcm_executor::traits::{validate_export, ExportXcm};
use SendError::*;

/// Returns the network ID and consensus location within that network of the remote
/// location `dest` which is itself specified as a location relative to the local
/// chain, itself situated at `universal_local` within the consensus universe. If
/// `dest` is not a location in remote consensus, then an error is returned.
pub fn ensure_is_remote(
	universal_local: impl Into<InteriorLocation>,
	dest: impl Into<Location>,
) -> Result<(NetworkId, InteriorLocation), Location> {
	let dest = dest.into();
	let universal_local = universal_local.into();
	let Ok(local_net) = universal_local.global_consensus() else { return Err(dest) };
	let universal_destination: InteriorLocation = universal_local
		.into_location()
		.appended_with(dest.clone())
		.map_err(|x| x.1)?
		.try_into()?;
	let (remote_dest, remote_net) = match universal_destination.split_first() {
		(d, Some(GlobalConsensus(n))) if n != local_net => (d, n),
		_ => return Err(dest),
	};
	Ok((remote_net, remote_dest))
}

/// Implementation of `SendXcm` which uses the given `ExportXcm` implementation in order to forward
/// the message over a bridge.
///
/// This is only useful when the local chain has bridging capabilities.
pub struct LocalExporter<Exporter, UniversalLocation>(PhantomData<(Exporter, UniversalLocation)>);
impl<Exporter: ExportXcm, UniversalLocation: Get<InteriorLocation>> SendXcm
	for LocalExporter<Exporter, UniversalLocation>
{
	type Ticket = Exporter::Ticket;

	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<Exporter::Ticket> {
		// This `clone` ensures that `dest` is not consumed in any case.
		let d = dest.clone().ok_or(MissingArgument)?;
		let universal_source = UniversalLocation::get();
		let devolved = ensure_is_remote(universal_source.clone(), d).map_err(|error| {
			tracing::debug!(target: "xcm::universal_exports", ?error, "Failed to devolve location");
			NotApplicable
		})?;
		let (remote_network, remote_location) = devolved;
		let xcm = msg.take().ok_or(MissingArgument)?;

		let hash =
			(Some(Location::here()), &remote_location).using_encoded(sp_io::hashing::blake2_128);
		let channel = u32::decode(&mut hash.as_ref()).unwrap_or(0);

		validate_export::<Exporter>(
			remote_network,
			channel,
			universal_source,
			remote_location,
			xcm.clone(),
		)
		.inspect_err(|err| {
			if let NotApplicable = err {
				// We need to make sure that msg is not consumed in case of `NotApplicable`.
				*msg = Some(xcm);
			}
		})
	}

	fn deliver(ticket: Exporter::Ticket) -> Result<XcmHash, SendError> {
		Exporter::deliver(ticket)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful_delivery(_: Option<Location>) {}
}

pub trait ExporterFor {
	/// Return the locally-routable bridge (if any) capable of forwarding `message` to the
	/// `remote_location` on the remote `network`, together with the payment which is required.
	///
	/// The payment is specified from the local context, not the bridge chain. This is the
	/// total amount to withdraw in to Holding and should cover both payment for the execution on
	/// the bridge chain and payment for the use of the `ExportMessage` instruction.
	fn exporter_for(
		network: &NetworkId,
		remote_location: &InteriorLocation,
		message: &Xcm<()>,
	) -> Option<(Location, Option<Asset>)>;
}

#[impl_trait_for_tuples::impl_for_tuples(30)]
impl ExporterFor for Tuple {
	fn exporter_for(
		network: &NetworkId,
		remote_location: &InteriorLocation,
		message: &Xcm<()>,
	) -> Option<(Location, Option<Asset>)> {
		for_tuples!( #(
			if let Some(r) = Tuple::exporter_for(network, remote_location, message) {
				return Some(r);
			}
		)* );
		None
	}
}

/// Configuration item representing a single exporter in the `NetworkExportTable`.
pub struct NetworkExportTableItem {
	/// Supported remote network.
	pub remote_network: NetworkId,
	/// Remote location filter.
	/// If `Some`, the requested remote location must be equal to one of the items in the vector.
	/// These are locations in the remote network.
	/// If `None`, then the check is skipped.
	pub remote_location_filter: Option<Vec<InteriorLocation>>,
	/// Locally-routable bridge with bridging capabilities to the `remote_network` and
	/// `remote_location`. See [`ExporterFor`] for more details.
	pub bridge: Location,
	/// The local payment.
	/// See [`ExporterFor`] for more details.
	pub payment: Option<Asset>,
}

impl NetworkExportTableItem {
	pub fn new(
		remote_network: NetworkId,
		remote_location_filter: Option<Vec<InteriorLocation>>,
		bridge: Location,
		payment: Option<Asset>,
	) -> Self {
		Self { remote_network, remote_location_filter, bridge, payment }
	}
}

/// An adapter for the implementation of `ExporterFor`, which attempts to find the
/// `(bridge_location, payment)` for the requested `network` and `remote_location` in the provided
/// `T` table containing various exporters.
pub struct NetworkExportTable<T>(core::marker::PhantomData<T>);
impl<T: Get<Vec<NetworkExportTableItem>>> ExporterFor for NetworkExportTable<T> {
	fn exporter_for(
		network: &NetworkId,
		remote_location: &InteriorLocation,
		_: &Xcm<()>,
	) -> Option<(Location, Option<Asset>)> {
		T::get()
			.into_iter()
			.find(|item| {
				&item.remote_network == network &&
					item.remote_location_filter
						.as_ref()
						.map(|filters| filters.iter().any(|filter| filter == remote_location))
						.unwrap_or(true)
			})
			.map(|item| (item.bridge, item.payment))
	}
}

/// Implementation of `SendXcm` which wraps the message inside an `ExportMessage` instruction
/// and sends it to a destination known to be able to handle it.
///
/// No effort is made to make payment to the bridge for its services, so the bridge location
/// must have been configured with a barrier rule allowing unpaid execution for this message
/// coming from our origin.
///
/// This is only useful if we have special dispensation by the remote bridges to have the
/// `ExportMessage` instruction executed without payment.
///
/// The `XcmHash` value returned by `deliver` will always be the same as that returned by the
/// message exporter (`Bridges`). Generally this should take notice of the message should it
/// end with the `SetTopic` instruction.
///
/// In the case that the message ends with a `SetTopic(T)` (as should be the case if the top-level
/// router is `WithUniqueTopic`), then the forwarding message (i.e. the one carrying the
/// export instruction *to* the bridge in local consensus) will also end with the same
/// `SetTopic(T)`. If this is not the case, then the onward message will not be given the `SetTopic`
/// afterward.
pub struct UnpaidRemoteExporter<Bridges, Router, UniversalLocation>(
	PhantomData<(Bridges, Router, UniversalLocation)>,
);
impl<Bridges: ExporterFor, Router: SendXcm, UniversalLocation: Get<InteriorLocation>> SendXcm
	for UnpaidRemoteExporter<Bridges, Router, UniversalLocation>
{
	type Ticket = Router::Ticket;

	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<Router::Ticket> {
		// This `clone` ensures that `dest` is not consumed in any case.
		let d = dest.clone().ok_or(MissingArgument)?;
		let devolved = ensure_is_remote(UniversalLocation::get(), d).map_err(|error| {
			tracing::debug!(target: "xcm::universal_exports", ?error, "Failed to devolve location");
			NotApplicable
		})?;
		let (remote_network, remote_location) = devolved;
		let xcm = msg.take().ok_or(MissingArgument)?;

		// find exporter
		let Some((bridge, maybe_payment)) =
			Bridges::exporter_for(&remote_network, &remote_location, &xcm)
		else {
			// We need to make sure that msg is not consumed in case of `NotApplicable`.
			*msg = Some(xcm);
			return Err(NotApplicable);
		};

		// `xcm` should already end with `SetTopic` - if it does, then extract and derive into
		// an onward topic ID.
		let maybe_forward_id = match xcm.last() {
			Some(SetTopic(t)) => Some(*t),
			_ => None,
		};

		// We then send a normal message to the bridge asking it to export the prepended
		// message to the remote chain. This will only work if the bridge will do the message
		// export for free. Common-good chains will typically be afforded this.
		let mut message = Xcm(vec![
			UnpaidExecution { weight_limit: Unlimited, check_origin: None },
			ExportMessage {
				network: remote_network,
				destination: remote_location,
				xcm: xcm.clone(),
			},
		]);
		if let Some(forward_id) = maybe_forward_id {
			message.0.push(SetTopic(forward_id));
		}
		let (v, mut cost) = validate_send::<Router>(bridge, message).inspect_err(|err| {
			if let NotApplicable = err {
				// We need to make sure that msg is not consumed in case of `NotApplicable`.
				*msg = Some(xcm);
			}
		})?;
		if let Some(bridge_payment) = maybe_payment {
			cost.push(bridge_payment);
		}
		Ok((v, cost))
	}

	fn deliver(validation: Self::Ticket) -> Result<XcmHash, SendError> {
		Router::deliver(validation)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful_delivery(location: Option<Location>) {
		Router::ensure_successful_delivery(location);
	}
}

impl<Bridges, Router, UniversalLocation> InspectMessageQueues
	for UnpaidRemoteExporter<Bridges, Router, UniversalLocation>
{
	fn clear_messages() {}

	/// This router needs to implement `InspectMessageQueues` but doesn't have to
	/// return any messages, since it just reuses the `XcmpQueue` router.
	fn get_messages() -> Vec<(VersionedLocation, Vec<VersionedXcm<()>>)> {
		Vec::new()
	}
}

/// Implementation of `SendXcm` which wraps the message inside an `ExportMessage` instruction
/// and sends it to a destination known to be able to handle it.
///
/// The `ExportMessage` instruction on the bridge is paid for from the local chain's sovereign
/// account on the bridge. The amount paid is determined through the `ExporterFor` trait.
///
/// The `XcmHash` value returned by `deliver` will always be the same as that returned by the
/// message exporter (`Bridges`). Generally this should take notice of the message should it
/// end with the `SetTopic` instruction.
///
/// In the case that the message ends with a `SetTopic(T)` (as should be the case if the top-level
/// router is `WithUniqueTopic`), then the forwarding message (i.e. the one carrying the
/// export instruction *to* the bridge in local consensus) will also end with the same
/// `SetTopic(T)`. If this is not the case, then the onward message will not be given the `SetTopic`
/// afterward.
pub struct SovereignPaidRemoteExporter<Bridges, Router, UniversalLocation>(
	PhantomData<(Bridges, Router, UniversalLocation)>,
);
impl<Bridges: ExporterFor, Router: SendXcm, UniversalLocation: Get<InteriorLocation>> SendXcm
	for SovereignPaidRemoteExporter<Bridges, Router, UniversalLocation>
{
	type Ticket = Router::Ticket;

	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<Router::Ticket> {
		// This `clone` ensures that `dest` is not consumed in any case.
		let d = dest.clone().ok_or(MissingArgument)?;
		let devolved = ensure_is_remote(UniversalLocation::get(), d).map_err(|error| {
			tracing::debug!(target: "xcm::universal_exports", ?error, "Failed to devolve location");
			NotApplicable
		})?;
		let (remote_network, remote_location) = devolved;
		let xcm = msg.take().ok_or(MissingArgument)?;

		// find exporter
		let Some((bridge, maybe_payment)) =
			Bridges::exporter_for(&remote_network, &remote_location, &xcm)
		else {
			// We need to make sure that msg is not consumed in case of `NotApplicable`.
			*msg = Some(xcm);
			return Err(NotApplicable);
		};

		// `xcm` should already end with `SetTopic` - if it does, then extract and derive into
		// an onward topic ID.
		let maybe_forward_id = match xcm.last() {
			Some(SetTopic(t)) => Some(*t),
			_ => None,
		};

		let local_from_bridge = UniversalLocation::get().invert_target(&bridge).map_err(|_| {
			tracing::debug!(target: "xcm::universal_exports", "Failed to invert bridge location");
			Unroutable
		})?;
		let export_instruction = ExportMessage {
			network: remote_network,
			destination: remote_location,
			xcm: xcm.clone(),
		};

		let mut message = Xcm(if let Some(ref payment) = maybe_payment {
			let fees =
				payment.clone().reanchored(&bridge, &UniversalLocation::get()).map_err(|_| {
					tracing::debug!(target: "xcm::universal_exports", "Failed to reanchor payment");
					Unroutable
				})?;
			vec![
				WithdrawAsset(fees.clone().into()),
				BuyExecution { fees, weight_limit: Unlimited },
				// `SetAppendix` ensures that `fees` are not trapped in any case, for example, when
				// `ExportXcm::validate` encounters an error during the processing of
				// `ExportMessage`.
				SetAppendix(Xcm(vec![DepositAsset {
					assets: AllCounted(1).into(),
					beneficiary: local_from_bridge,
				}])),
				export_instruction,
			]
		} else {
			vec![export_instruction]
		});
		if let Some(forward_id) = maybe_forward_id {
			message.0.push(SetTopic(forward_id));
		}

		// We then send a normal message to the bridge asking it to export the prepended
		// message to the remote chain.
		let (v, mut cost) = validate_send::<Router>(bridge, message).inspect_err(|err| {
			if let NotApplicable = err {
				// We need to make sure that msg is not consumed in case of `NotApplicable`.
				*msg = Some(xcm);
			}
		})?;
		if let Some(bridge_payment) = maybe_payment {
			cost.push(bridge_payment);
		}
		Ok((v, cost))
	}

	fn deliver(ticket: Router::Ticket) -> Result<XcmHash, SendError> {
		Router::deliver(ticket)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful_delivery(location: Option<Location>) {
		Router::ensure_successful_delivery(location);
	}
}

impl<Bridges, Router, UniversalLocation> InspectMessageQueues
	for SovereignPaidRemoteExporter<Bridges, Router, UniversalLocation>
{
	fn clear_messages() {}

	/// This router needs to implement `InspectMessageQueues` but doesn't have to
	/// return any messages, since it just reuses the `XcmpQueue` router.
	fn get_messages() -> Vec<(VersionedLocation, Vec<VersionedXcm<()>>)> {
		Vec::new()
	}
}

pub trait DispatchBlob {
	/// Takes an incoming blob from over some point-to-point link (usually from some sort of
	/// inter-consensus bridge) and then does what needs to be done with it. Usually this means
	/// forwarding it on into some other location sharing our consensus or possibly just enqueuing
	/// it for execution locally if it is destined for the local chain.
	///
	/// NOTE: The API does not provide for any kind of weight or fee management; the size of the
	/// `blob` is known to the caller and so the operation must have a linear weight relative to
	/// `blob`'s length. This means that you will generally only want to **enqueue** the blob, not
	/// enact it. Fees must be handled by the caller.
	fn dispatch_blob(blob: Vec<u8>) -> Result<(), DispatchBlobError>;
}

pub trait HaulBlob {
	/// Sends a blob over some point-to-point link. This will generally be implemented by a bridge.
	fn haul_blob(blob: Vec<u8>) -> Result<(), HaulBlobError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HaulBlobError {
	/// Represents point-to-point link failure with a human-readable explanation of the specific
	/// issue is provided.
	Transport(&'static str),
}

impl From<HaulBlobError> for SendError {
	fn from(err: HaulBlobError) -> Self {
		match err {
			HaulBlobError::Transport(reason) => SendError::Transport(reason),
		}
	}
}

#[derive(Clone, Encode, Decode)]
pub struct BridgeMessage {
	/// The message destination as a *Universal Location*. This means it begins with a
	/// `GlobalConsensus` junction describing the network under which global consensus happens.
	/// If this does not match our global consensus then it's a fatal error.
	pub universal_dest: VersionedInteriorLocation,
	pub message: VersionedXcm<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchBlobError {
	Unbridgable,
	InvalidEncoding,
	UnsupportedLocationVersion,
	UnsupportedXcmVersion,
	RoutingError,
	NonUniversalDestination,
	WrongGlobal,
}

pub struct BridgeBlobDispatcher<Router, OurPlace, OurPlaceBridgeInstance>(
	PhantomData<(Router, OurPlace, OurPlaceBridgeInstance)>,
);
impl<
		Router: SendXcm,
		OurPlace: Get<InteriorLocation>,
		OurPlaceBridgeInstance: Get<Option<InteriorLocation>>,
	> DispatchBlob for BridgeBlobDispatcher<Router, OurPlace, OurPlaceBridgeInstance>
{
	fn dispatch_blob(blob: Vec<u8>) -> Result<(), DispatchBlobError> {
		let our_universal = OurPlace::get();
		let our_global = our_universal.global_consensus().map_err(|()| {
			tracing::debug!(target: "xcm::universal_exports", "Failed to get global consensus");
			DispatchBlobError::Unbridgable
		})?;
		let BridgeMessage { universal_dest, message } =
			Decode::decode(&mut &blob[..]).map_err(|error| {
				tracing::debug!(target: "xcm::universal_exports", ?error, "Failed to decode blob");
				DispatchBlobError::InvalidEncoding
			})?;
		let universal_dest: InteriorLocation = universal_dest.try_into().map_err(|_| {
			tracing::debug!(target: "xcm::universal_exports", "Failed to convert universal destination");
			DispatchBlobError::UnsupportedLocationVersion
		})?;
		// `universal_dest` is the desired destination within the universe: first we need to check
		// we're in the right global consensus.
		let intended_global = universal_dest
			.global_consensus()
			.map_err(|()| {
				tracing::debug!(target: "xcm::universal_exports", "Failed to get global consensus from universal destination");
				DispatchBlobError::NonUniversalDestination })?;
		ensure!(intended_global == our_global, DispatchBlobError::WrongGlobal);
		let dest = universal_dest.relative_to(&our_universal);
		let mut message: Xcm<()> = message.try_into().map_err(|_| {
			tracing::debug!(target: "xcm::universal_exports", "Failed to convert message");
			DispatchBlobError::UnsupportedXcmVersion
		})?;

		// Prepend our bridge instance discriminator.
		// Can be used for fine-grained control of origin on destination in case of multiple bridge
		// instances, e.g. restrict `type UniversalAliases` and `UniversalOrigin` instruction to
		// trust just particular bridge instance for `NetworkId`.
		if let Some(bridge_instance) = OurPlaceBridgeInstance::get() {
			message.0.insert(0, DescendOrigin(bridge_instance));
		}

		send_xcm::<Router>(dest, message).map_err(|error| {
			tracing::debug!(target: "xcm::universal_exports", ?error, "Failed to send XCM");
			DispatchBlobError::RoutingError
		})?;
		Ok(())
	}
}

pub struct HaulBlobExporter<Bridge, BridgedNetwork, DestinationVersion, Price>(
	PhantomData<(Bridge, BridgedNetwork, DestinationVersion, Price)>,
);
/// `ExportXcm` implementation for `HaulBlobExporter`.
///
/// # Type Parameters
///
/// ```text
/// - Bridge: Implements `HaulBlob`.
/// - BridgedNetwork: The relative location of the bridged consensus system with the expected `GlobalConsensus` junction.
/// - DestinationVersion: Implements `GetVersion` for retrieving XCM version for the destination.
/// - Price: potential fees for exporting.
/// ```
impl<
		Bridge: HaulBlob,
		BridgedNetwork: Get<Location>,
		DestinationVersion: GetVersion,
		Price: Get<Assets>,
	> ExportXcm for HaulBlobExporter<Bridge, BridgedNetwork, DestinationVersion, Price>
{
	type Ticket = (Vec<u8>, XcmHash);

	fn validate(
		network: NetworkId,
		_channel: u32,
		universal_source: &mut Option<InteriorLocation>,
		destination: &mut Option<InteriorLocation>,
		message: &mut Option<Xcm<()>>,
	) -> Result<((Vec<u8>, XcmHash), Assets), SendError> {
		let (bridged_network, bridged_network_location_parents) = {
			let Location { parents, interior: mut junctions } = BridgedNetwork::get();
			match junctions.take_first() {
				Some(GlobalConsensus(network)) => (network, parents),
				_ => return Err(NotApplicable),
			}
		};
		ensure!(&network == &bridged_network, NotApplicable);
		// We don't/can't use the `channel` for this adapter.
		let dest = destination.take().ok_or(SendError::MissingArgument)?;

		// Let's resolve the known/supported XCM version for the destination because we don't know
		// if it supports the same/latest version.
		let (universal_dest, version) =
			match dest.pushed_front_with(GlobalConsensus(bridged_network)) {
				Ok(d) => {
					let version = DestinationVersion::get_version_for(&Location::from(
						AncestorThen(bridged_network_location_parents, d.clone()),
					))
					.ok_or(SendError::DestinationUnsupported)?;
					(d, version)
				},
				Err((dest, _)) => {
					*destination = Some(dest);
					return Err(NotApplicable);
				},
			};

		// Let's adjust XCM with `UniversalOrigin`, `DescendOrigin` and`SetTopic`.
		let (local_net, local_sub) = universal_source
			.take()
			.ok_or(SendError::MissingArgument)?
			.split_global()
			.map_err(|()| {
				tracing::debug!(target: "xcm::universal_exports", "Failed to split global consensus");
				SendError::Unroutable
			})?;
		let mut message = message.take().ok_or(SendError::MissingArgument)?;
		let maybe_id = match message.last() {
			Some(SetTopic(t)) => Some(*t),
			_ => None,
		};
		message.0.insert(0, UniversalOrigin(GlobalConsensus(local_net)));
		if local_sub != Here {
			message.0.insert(1, DescendOrigin(local_sub));
		}

		// We cannot use the latest `Versioned` because we don't know if the target chain already
		// supports the same version. Therefore, we better control the destination version with best
		// efforts.
		let message = VersionedXcm::from(message).into_version(version).map_err(|()| {
			tracing::debug!(target: "xcm::universal_exports", "Failed to convert message to versioned XCM");
			SendError::DestinationUnsupported
		})?;
		let universal_dest = VersionedInteriorLocation::from(universal_dest)
			.into_version(version)
			.map_err(|()| {
				tracing::debug!(target: "xcm::universal_exports", "Failed to convert destination to versioned location");
				SendError::DestinationUnsupported })?;

		let id = maybe_id.unwrap_or_else(|| message.using_encoded(sp_io::hashing::blake2_256));
		let blob = BridgeMessage { universal_dest, message }.encode();
		Ok(((blob, id), Price::get()))
	}

	fn deliver((blob, id): (Vec<u8>, XcmHash)) -> Result<XcmHash, SendError> {
		Bridge::haul_blob(blob)?;
		Ok(id)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_support::{
		assert_err, assert_ok,
		traits::{Contains, Equals},
	};

	#[test]
	fn ensure_is_remote_works() {
		// A Kusama parachain is remote from the Polkadot Relay.
		let x = ensure_is_remote(Polkadot, (Parent, Kusama, Parachain(1000)));
		assert_eq!(x, Ok((Kusama, Parachain(1000).into())));

		// Polkadot Relay is remote from a Kusama parachain.
		let x = ensure_is_remote((Kusama, Parachain(1000)), (Parent, Parent, Polkadot));
		assert_eq!(x, Ok((Polkadot, Here)));

		// Our own parachain is local.
		let x = ensure_is_remote(Polkadot, Parachain(1000));
		assert_eq!(x, Err(Parachain(1000).into()));

		// Polkadot's parachain is not remote if we are Polkadot.
		let x = ensure_is_remote(Polkadot, (Parent, Polkadot, Parachain(1000)));
		assert_eq!(x, Err((Parent, Polkadot, Parachain(1000)).into()));

		// If we don't have a consensus ancestor, then we cannot determine remoteness.
		let x = ensure_is_remote((), (Parent, Polkadot, Parachain(1000)));
		assert_eq!(x, Err((Parent, Polkadot, Parachain(1000)).into()));
	}

	pub struct OkFor<Filter>(PhantomData<Filter>);
	impl<Filter: Contains<Location>> SendXcm for OkFor<Filter> {
		type Ticket = ();

		fn validate(
			destination: &mut Option<Location>,
			_message: &mut Option<Xcm<()>>,
		) -> SendResult<Self::Ticket> {
			if let Some(d) = destination.as_ref() {
				if Filter::contains(&d) {
					return Ok(((), Assets::new()));
				}
			}
			Err(NotApplicable)
		}

		fn deliver(_ticket: Self::Ticket) -> Result<XcmHash, SendError> {
			Ok([0; 32])
		}

		#[cfg(feature = "runtime-benchmarks")]
		fn ensure_successful_delivery(_: Option<Location>) {}
	}
	impl<Filter: Contains<(NetworkId, InteriorLocation)>> ExportXcm for OkFor<Filter> {
		type Ticket = ();

		fn validate(
			network: NetworkId,
			_: u32,
			_: &mut Option<InteriorLocation>,
			destination: &mut Option<InteriorLocation>,
			_: &mut Option<Xcm<()>>,
		) -> SendResult<Self::Ticket> {
			if let Some(d) = destination.as_ref() {
				if Filter::contains(&(network, d.clone())) {
					return Ok(((), Assets::new()));
				}
			}
			Err(NotApplicable)
		}

		fn deliver(_ticket: Self::Ticket) -> Result<XcmHash, SendError> {
			Ok([1; 32])
		}
	}

	/// Generic test case asserting that dest and msg is not consumed by `validate` implementation
	/// of `SendXcm` in case of expected result.
	fn ensure_validate_does_not_consume_dest_or_msg<S: SendXcm>(
		dest: Location,
		assert_result: impl Fn(SendResult<S::Ticket>),
	) {
		let mut dest_wrapper = Some(dest.clone());
		let msg = Xcm::<()>::new();
		let mut msg_wrapper = Some(msg.clone());

		assert_result(S::validate(&mut dest_wrapper, &mut msg_wrapper));

		// ensure dest and msg are untouched
		assert_eq!(Some(dest), dest_wrapper);
		assert_eq!(Some(msg), msg_wrapper);
	}

	#[test]
	fn local_exporters_works() {
		frame_support::parameter_types! {
			pub Local: NetworkId = ByGenesis([0; 32]);
			pub UniversalLocation: InteriorLocation = [GlobalConsensus(Local::get()), Parachain(1234)].into();
			pub DifferentRemote: NetworkId = ByGenesis([22; 32]);
			pub RemoteDestination: Junction = Parachain(9657);
			pub RoutableBridgeFilter: (NetworkId, InteriorLocation) = (DifferentRemote::get(), RemoteDestination::get().into());
		}
		type RoutableBridgeExporter = OkFor<Equals<RoutableBridgeFilter>>;
		type NotApplicableBridgeExporter = OkFor<()>;
		assert_ok!(validate_export::<RoutableBridgeExporter>(
			DifferentRemote::get(),
			0,
			UniversalLocation::get(),
			RemoteDestination::get().into(),
			Xcm::default()
		));
		assert_err!(
			validate_export::<NotApplicableBridgeExporter>(
				DifferentRemote::get(),
				0,
				UniversalLocation::get(),
				RemoteDestination::get().into(),
				Xcm::default()
			),
			NotApplicable
		);

		// 1. check with local destination (should be remote)
		let local_dest: Location = (Parent, Parachain(5678)).into();
		assert!(ensure_is_remote(UniversalLocation::get(), local_dest.clone()).is_err());

		// LocalExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			LocalExporter<RoutableBridgeExporter, UniversalLocation>,
		>(local_dest.clone(), |result| assert_eq!(Err(NotApplicable), result));

		// 2. check with not applicable from the inner router (using `NotApplicableBridgeSender`)
		let remote_dest: Location =
			(Parent, Parent, DifferentRemote::get(), RemoteDestination::get()).into();
		assert!(ensure_is_remote(UniversalLocation::get(), remote_dest.clone()).is_ok());

		// LocalExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			LocalExporter<NotApplicableBridgeExporter, UniversalLocation>,
		>(remote_dest.clone(), |result| assert_eq!(Err(NotApplicable), result));

		// 3. Ok - deliver
		// UnpaidRemoteExporter
		assert_ok!(send_xcm::<LocalExporter<RoutableBridgeExporter, UniversalLocation>>(
			remote_dest,
			Xcm::default()
		));
	}

	#[test]
	fn remote_exporters_works() {
		frame_support::parameter_types! {
			pub Local: NetworkId = ByGenesis([0; 32]);
			pub UniversalLocation: InteriorLocation = [GlobalConsensus(Local::get()), Parachain(1234)].into();
			pub DifferentRemote: NetworkId = ByGenesis([22; 32]);
			pub RoutableBridge: Location = Location::new(1, Parachain(9657));
			// not routable
			pub NotApplicableBridgeTable: Vec<NetworkExportTableItem> = vec![];
			// routable
			pub RoutableBridgeTable: Vec<NetworkExportTableItem> = vec![
				NetworkExportTableItem::new(
					DifferentRemote::get(),
					None,
					RoutableBridge::get(),
					None
				)
			];
		}
		type RoutableBridgeSender = OkFor<Equals<RoutableBridge>>;
		type NotApplicableBridgeSender = OkFor<()>;
		assert_ok!(validate_send::<RoutableBridgeSender>(RoutableBridge::get(), Xcm::default()));
		assert_err!(
			validate_send::<NotApplicableBridgeSender>(RoutableBridge::get(), Xcm::default()),
			NotApplicable
		);

		// 1. check with local destination (should be remote)
		let local_dest: Location = (Parent, Parachain(5678)).into();
		assert!(ensure_is_remote(UniversalLocation::get(), local_dest.clone()).is_err());

		// UnpaidRemoteExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			UnpaidRemoteExporter<
				NetworkExportTable<RoutableBridgeTable>,
				RoutableBridgeSender,
				UniversalLocation,
			>,
		>(local_dest.clone(), |result| assert_eq!(Err(NotApplicable), result));
		// SovereignPaidRemoteExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			SovereignPaidRemoteExporter<
				NetworkExportTable<RoutableBridgeTable>,
				RoutableBridgeSender,
				UniversalLocation,
			>,
		>(local_dest, |result| assert_eq!(Err(NotApplicable), result));

		// 2. check with not applicable destination (`NotApplicableBridgeTable`)
		let remote_dest: Location = (Parent, Parent, DifferentRemote::get()).into();
		assert!(ensure_is_remote(UniversalLocation::get(), remote_dest.clone()).is_ok());

		// UnpaidRemoteExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			UnpaidRemoteExporter<
				NetworkExportTable<NotApplicableBridgeTable>,
				RoutableBridgeSender,
				UniversalLocation,
			>,
		>(remote_dest.clone(), |result| assert_eq!(Err(NotApplicable), result));
		// SovereignPaidRemoteExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			SovereignPaidRemoteExporter<
				NetworkExportTable<NotApplicableBridgeTable>,
				RoutableBridgeSender,
				UniversalLocation,
			>,
		>(remote_dest, |result| assert_eq!(Err(NotApplicable), result));

		// 3. check with not applicable from the inner router (using `NotApplicableBridgeSender`)
		let remote_dest: Location = (Parent, Parent, DifferentRemote::get()).into();
		assert!(ensure_is_remote(UniversalLocation::get(), remote_dest.clone()).is_ok());

		// UnpaidRemoteExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			UnpaidRemoteExporter<
				NetworkExportTable<RoutableBridgeTable>,
				NotApplicableBridgeSender,
				UniversalLocation,
			>,
		>(remote_dest.clone(), |result| assert_eq!(Err(NotApplicable), result));
		// SovereignPaidRemoteExporter
		ensure_validate_does_not_consume_dest_or_msg::<
			SovereignPaidRemoteExporter<
				NetworkExportTable<RoutableBridgeTable>,
				NotApplicableBridgeSender,
				UniversalLocation,
			>,
		>(remote_dest.clone(), |result| assert_eq!(Err(NotApplicable), result));

		// 4. Ok - deliver
		// UnpaidRemoteExporter
		assert_ok!(send_xcm::<
			UnpaidRemoteExporter<
				NetworkExportTable<RoutableBridgeTable>,
				RoutableBridgeSender,
				UniversalLocation,
			>,
		>(remote_dest.clone(), Xcm::default()));
		// SovereignPaidRemoteExporter
		assert_ok!(send_xcm::<
			SovereignPaidRemoteExporter<
				NetworkExportTable<RoutableBridgeTable>,
				RoutableBridgeSender,
				UniversalLocation,
			>,
		>(remote_dest, Xcm::default()));
	}

	#[test]
	fn network_export_table_works() {
		frame_support::parameter_types! {
			pub NetworkA: NetworkId = ByGenesis([0; 32]);
			pub Parachain1000InNetworkA: InteriorLocation = [Parachain(1000)].into();
			pub Parachain2000InNetworkA: InteriorLocation = [Parachain(2000)].into();

			pub NetworkB: NetworkId = ByGenesis([1; 32]);

			pub BridgeToALocation: Location = Location::new(1, [Parachain(1234)]);
			pub BridgeToBLocation: Location = Location::new(1, [Parachain(4321)]);

			pub PaymentForNetworkAAndParachain2000: Asset = (Location::parent(), 150).into();

			pub BridgeTable: alloc::vec::Vec<NetworkExportTableItem> = alloc::vec![
				// NetworkA allows `Parachain(1000)` as remote location WITHOUT payment.
				NetworkExportTableItem::new(
					NetworkA::get(),
					Some(vec![Parachain1000InNetworkA::get()]),
					BridgeToALocation::get(),
					None
				),
				// NetworkA allows `Parachain(2000)` as remote location WITH payment.
				NetworkExportTableItem::new(
					NetworkA::get(),
					Some(vec![Parachain2000InNetworkA::get()]),
					BridgeToALocation::get(),
					Some(PaymentForNetworkAAndParachain2000::get())
				),
				// NetworkB allows all remote location.
				NetworkExportTableItem::new(
					NetworkB::get(),
					None,
					BridgeToBLocation::get(),
					None
				)
			];
		}

		let test_data: Vec<(NetworkId, InteriorLocation, Option<(Location, Option<Asset>)>)> = vec![
			(NetworkA::get(), [Parachain(1000)].into(), Some((BridgeToALocation::get(), None))),
			(NetworkA::get(), [Parachain(1000), GeneralIndex(1)].into(), None),
			(
				NetworkA::get(),
				[Parachain(2000)].into(),
				Some((BridgeToALocation::get(), Some(PaymentForNetworkAAndParachain2000::get()))),
			),
			(NetworkA::get(), [Parachain(2000), GeneralIndex(1)].into(), None),
			(NetworkA::get(), [Parachain(3000)].into(), None),
			(NetworkB::get(), [Parachain(1000)].into(), Some((BridgeToBLocation::get(), None))),
			(NetworkB::get(), [Parachain(2000)].into(), Some((BridgeToBLocation::get(), None))),
			(NetworkB::get(), [Parachain(3000)].into(), Some((BridgeToBLocation::get(), None))),
		];

		for (network, remote_location, expected_result) in test_data {
			assert_eq!(
				NetworkExportTable::<BridgeTable>::exporter_for(
					&network,
					&remote_location,
					&Xcm::default()
				),
				expected_result,
				"expected_result: {:?} not matched for network: {:?} and remote_location: {:?}",
				expected_result,
				network,
				remote_location,
			)
		}
	}
}

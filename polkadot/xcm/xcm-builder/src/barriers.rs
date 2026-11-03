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

//! Various implementations for `ShouldExecute`.

use crate::{CreateMatcher, MatchXcm};
use core::{cell::Cell, marker::PhantomData, ops::ControlFlow, result::Result};
use frame_support::{
	ensure,
	traits::{Contains, ContainsPair, Get, Nothing, ProcessMessageError},
};
use polkadot_parachain_primitives::primitives::IsSystem;
use xcm::prelude::*;
use xcm_executor::traits::{CheckSuspension, DenyExecution, OnResponse, Properties, ShouldExecute};

/// Execution barrier that just takes `max_weight` from `properties.weight_credit`.
///
/// Useful to allow XCM execution by local chain users via extrinsics.
/// E.g. `pallet_xcm::reserve_asset_transfer` to transfer a reserve asset
/// out of the local chain to another one.
pub struct TakeWeightCredit;
impl ShouldExecute for TakeWeightCredit {
	fn should_execute<RuntimeCall>(
		origin: &Location,
		instructions: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin,
			?instructions,
			?max_weight,
			?properties,
			"TakeWeightCredit"
		);
		properties.weight_credit = properties
			.weight_credit
			.checked_sub(&max_weight)
			.ok_or(ProcessMessageError::Overweight(max_weight))?;
		Ok(())
	}
}

const MAX_ASSETS_FOR_BUY_EXECUTION: usize = 2;

/// Allows execution from `origin` if it is contained in `T` (i.e. `T::Contains(origin)`) taking
/// payments into account.
///
/// Only allows for `WithdrawAsset`, `ReceiveTeleportedAsset`, `ReserveAssetDeposited` and
/// `ClaimAsset` XCMs because they are the only ones that place assets in the Holding Register to
/// pay for execution.
pub struct AllowTopLevelPaidExecutionFrom<T>(PhantomData<T>);
impl<T: Contains<Location>> ShouldExecute for AllowTopLevelPaidExecutionFrom<T> {
	fn should_execute<RuntimeCall>(
		origin: &Location,
		instructions: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin,
			?instructions,
			?max_weight,
			?properties,
			"AllowTopLevelPaidExecutionFrom",
		);

		ensure!(T::contains(origin), ProcessMessageError::Unsupported);
		// We will read up to 5 instructions. This allows up to 3 `ClearOrigin` instructions. We
		// allow for more than one since anything beyond the first is a no-op and it's conceivable
		// that composition of operations might result in more than one being appended.
		let end = instructions.len().min(5);
		instructions[..end]
			.matcher()
			.match_next_inst(|inst| match inst {
				WithdrawAsset(ref assets) |
				ReceiveTeleportedAsset(ref assets) |
				ReserveAssetDeposited(ref assets) |
				ClaimAsset { ref assets, .. } => {
					if assets.len() <= MAX_ASSETS_FOR_BUY_EXECUTION {
						Ok(())
					} else {
						Err(ProcessMessageError::BadFormat)
					}
				},
				_ => Err(ProcessMessageError::BadFormat),
			})?
			.skip_inst_while(|inst| {
				matches!(inst, ClearOrigin | AliasOrigin(..)) ||
					matches!(inst, DescendOrigin(child) if child != &Here) ||
					matches!(inst, SetHints { .. })
			})?
			.match_next_inst(|inst| match inst {
				BuyExecution { weight_limit: Limited(ref mut weight), .. }
					if weight.all_gte(max_weight) =>
				{
					*weight = max_weight;
					Ok(())
				},
				BuyExecution { ref mut weight_limit, .. } if weight_limit == &Unlimited => {
					*weight_limit = Limited(max_weight);
					Ok(())
				},
				PayFees { .. } => Ok(()),
				_ => Err(ProcessMessageError::Overweight(max_weight)),
			})?;
		Ok(())
	}
}

/// A derivative barrier, which scans the first `MaxPrefixes` instructions for origin-alterers and
/// then evaluates `should_execute` of the `InnerBarrier` based on the remaining instructions and
/// the newly computed origin.
///
/// This effectively allows for the possibility of distinguishing an origin which is acting as a
/// router for its derivative locations (or as a bridge for a remote location) and an origin which
/// is actually trying to send a message for itself. In the former case, the message will be
/// prefixed with origin-mutating instructions.
///
/// Any barriers which should be interpreted based on the computed origin rather than the original
/// message origin should be subject to this. This is the case for most barriers since the
/// effective origin is generally more important than the routing origin. Any other barriers, and
/// especially those which should be interpreted only the routing origin should not be subject to
/// this.
///
/// E.g.
/// ```nocompile
/// type MyBarrier = (
/// 	TakeWeightCredit,
/// 	AllowTopLevelPaidExecutionFrom<DirectCustomerLocations>,
/// 	WithComputedOrigin<(
/// 		AllowTopLevelPaidExecutionFrom<DerivativeCustomerLocations>,
/// 		AllowUnpaidExecutionFrom<ParentLocation>,
/// 		AllowSubscriptionsFrom<AllowedSubscribers>,
/// 		AllowKnownQueryResponses<TheResponseHandler>,
/// 	)>,
/// );
/// ```
///
/// In the above example, `AllowUnpaidExecutionFrom` appears once underneath
/// `WithComputedOrigin`. This is in order to distinguish between messages which are notionally
/// from a derivative location of `ParentLocation` but that just happened to be sent via
/// `ParentLocation` rather than messages that were sent by the parent.
///
/// Similarly `AllowTopLevelPaidExecutionFrom` appears twice: once inside of `WithComputedOrigin`
/// where we provide the list of origins which are derivative origins, and then secondly outside
/// of `WithComputedOrigin` where we provide the list of locations which are direct origins. It's
/// reasonable for these lists to be merged into one and that used both inside and out.
///
/// Finally, we see `AllowSubscriptionsFrom` and `AllowKnownQueryResponses` are both inside of
/// `WithComputedOrigin`. This means that if a message begins with origin-mutating instructions,
/// then it must be the finally computed origin which we accept subscriptions or expect a query
/// response from. For example, even if an origin appeared in the `AllowedSubscribers` list, we
/// would ignore this rule if it began with origin mutators and they changed the origin to something
/// which was not on the list.
pub struct WithComputedOrigin<InnerBarrier, LocalUniversal, MaxPrefixes>(
	PhantomData<(InnerBarrier, LocalUniversal, MaxPrefixes)>,
);
impl<InnerBarrier: ShouldExecute, LocalUniversal: Get<InteriorLocation>, MaxPrefixes: Get<u32>>
	ShouldExecute for WithComputedOrigin<InnerBarrier, LocalUniversal, MaxPrefixes>
{
	fn should_execute<Call>(
		origin: &Location,
		instructions: &mut [Instruction<Call>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin,
			?instructions,
			?max_weight,
			?properties,
			"WithComputedOrigin"
		);
		let mut actual_origin = origin.clone();
		let skipped = Cell::new(0usize);
		// NOTE: We do not check the validity of `UniversalOrigin` here, meaning that a malicious
		// origin could place a `UniversalOrigin` in order to spoof some location which gets free
		// execution. This technical could get it past the barrier condition, but the execution
		// would instantly fail since the first instruction would cause an error with the
		// invalid UniversalOrigin.
		instructions.matcher().match_next_inst_while(
			|_| skipped.get() < MaxPrefixes::get() as usize,
			|inst| {
				match inst {
					UniversalOrigin(new_global) => {
						// Note the origin is *relative to local consensus*! So we need to escape
						// local consensus with the `parents` before diving in into the
						// `universal_location`.
						actual_origin =
							Junctions::from([*new_global]).relative_to(&LocalUniversal::get());
					},
					DescendOrigin(j) => {
						let Ok(_) = actual_origin.append_with(j.clone()) else {
							return Err(ProcessMessageError::Unsupported);
						};
					},
					_ => return Ok(ControlFlow::Break(())),
				};
				skipped.set(skipped.get() + 1);
				Ok(ControlFlow::Continue(()))
			},
		)?;
		InnerBarrier::should_execute(
			&actual_origin,
			&mut instructions[skipped.get()..],
			max_weight,
			properties,
		)
	}
}

/// Sets the message ID to `t` using a `SetTopic(t)` in the last position if present.
///
/// Note that the message ID does not necessarily have to be unique; it is the
/// sender's responsibility to ensure uniqueness.
///
/// Requires some inner barrier to pass on the rest of the message.
pub struct TrailingSetTopicAsId<InnerBarrier>(PhantomData<InnerBarrier>);
impl<InnerBarrier: ShouldExecute> ShouldExecute for TrailingSetTopicAsId<InnerBarrier> {
	fn should_execute<Call>(
		origin: &Location,
		instructions: &mut [Instruction<Call>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin,
			?instructions,
			?max_weight,
			?properties,
			"TrailingSetTopicAsId"
		);
		let until = if let Some(SetTopic(t)) = instructions.last() {
			properties.message_id = Some(*t);
			instructions.len() - 1
		} else {
			instructions.len()
		};
		InnerBarrier::should_execute(&origin, &mut instructions[..until], max_weight, properties)
	}
}

/// Barrier condition that allows for a `SuspensionChecker` that controls whether or not the XCM
/// executor will be suspended from executing the given XCM.
pub struct RespectSuspension<Inner, SuspensionChecker>(PhantomData<(Inner, SuspensionChecker)>);
impl<Inner, SuspensionChecker> ShouldExecute for RespectSuspension<Inner, SuspensionChecker>
where
	Inner: ShouldExecute,
	SuspensionChecker: CheckSuspension,
{
	fn should_execute<Call>(
		origin: &Location,
		instructions: &mut [Instruction<Call>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		if SuspensionChecker::is_suspended(origin, instructions, max_weight, properties) {
			Err(ProcessMessageError::Yield)
		} else {
			Inner::should_execute(origin, instructions, max_weight, properties)
		}
	}
}

/// Allows execution from any origin that is contained in `T` (i.e. `T::Contains(origin)`).
///
/// Use only for executions from completely trusted origins, from which no permissionless messages
/// can be sent.
pub struct AllowUnpaidExecutionFrom<T>(PhantomData<T>);
impl<T: Contains<Location>> ShouldExecute for AllowUnpaidExecutionFrom<T> {
	fn should_execute<RuntimeCall>(
		origin: &Location,
		instructions: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin, ?instructions, ?max_weight, ?properties,
			"AllowUnpaidExecutionFrom"
		);
		ensure!(T::contains(origin), ProcessMessageError::Unsupported);
		Ok(())
	}
}

/// Allows execution from any origin that is contained in `T` (i.e. `T::Contains(origin)`) if the
/// message explicitly includes the `UnpaidExecution` instruction.
///
/// Use only for executions from trusted origin groups.
///
/// Allows for the message to receive teleports or reserve asset transfers and altering
/// the origin before indicating `UnpaidExecution`.
///
/// Origin altering instructions are executed so the barrier can more accurately reject messages
/// whose effective origin at the time of calling `UnpaidExecution` is not allowed.
/// This means `T` will be checked against the actual origin _after_ being modified by prior
/// instructions.
///
/// In order to allow messages to use `AliasOrigin` before `UnpaidExecution`, the `Aliasers` type
/// should be set to a *cheap, computation-only* subset of `xcm_executor::Config::Aliasers`.
/// It must never include filters that read storage, such as `pallet_xcm::AuthorizedAliasers`:
/// barriers run before any payment is taken, so an expensive check here is a free-of-charge load
/// on the chain that any incoming message can trigger.
///
/// Aliases that are only allowed by the excluded (expensive) filters simply won't get unpaid
/// execution through this barrier; they can still buy execution via the paid barrier.
///
/// With the default (`Nothing`), all messages with an `AliasOrigin` instruction will be rejected.
pub struct AllowExplicitUnpaidExecutionFrom<T, Aliasers = Nothing>(PhantomData<(T, Aliasers)>);
impl<T: Contains<Location>, Aliasers: ContainsPair<Location, Location>> ShouldExecute
	for AllowExplicitUnpaidExecutionFrom<T, Aliasers>
{
	fn should_execute<Call>(
		origin: &Location,
		instructions: &mut [Instruction<Call>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin, ?instructions, ?max_weight, ?properties,
			"AllowExplicitUnpaidExecutionFrom",
		);
		// We will read up to 5 instructions before `UnpaidExecution`.
		// This allows up to 3 asset transfer instructions, thus covering all possible transfer
		// types, followed by a potential origin altering instruction, and a potential `SetHints`.
		let mut actual_origin = origin.clone();
		let processed = Cell::new(0usize);
		let instructions_to_process = 5;
		instructions
			.matcher()
			// We skip set hints and all types of asset transfer instructions.
			.match_next_inst_while(
				|inst| {
					processed.get() < instructions_to_process &&
						matches!(
							inst,
							ReceiveTeleportedAsset(_) |
								ReserveAssetDeposited(_) | WithdrawAsset(_) |
								SetHints { .. }
						)
				},
				|_| {
					processed.set(processed.get() + 1);
					Ok(ControlFlow::Continue(()))
				},
			)?
			// Then we go through all origin altering instructions and we
			// alter the original origin.
			.match_next_inst_while(
				|_| processed.get() < instructions_to_process,
				|inst| {
					match inst {
						ClearOrigin => {
							// We don't support the `ClearOrigin` instruction since we always need
							// to know the origin to know if it's allowed unpaid execution.
							return Err(ProcessMessageError::Unsupported);
						},
						AliasOrigin(target) => {
							if Aliasers::contains(&actual_origin, &target) {
								actual_origin = target.clone();
							} else {
								return Err(ProcessMessageError::Unsupported);
							}
						},
						DescendOrigin(child) if child != &Here => {
							let Ok(_) = actual_origin.append_with(child.clone()) else {
								return Err(ProcessMessageError::Unsupported);
							};
						},
						_ => return Ok(ControlFlow::Break(())),
					};
					processed.set(processed.get() + 1);
					Ok(ControlFlow::Continue(()))
				},
			)?
			// We finally match on the required `UnpaidExecution` instruction.
			.match_next_inst(|inst| match inst {
				UnpaidExecution { weight_limit: Limited(m), .. } if m.all_gte(max_weight) => Ok(()),
				UnpaidExecution { weight_limit: Unlimited, .. } => Ok(()),
				_ => Err(ProcessMessageError::Overweight(max_weight)),
			})?;

		// After processing all the instructions, `actual_origin` was modified and we
		// check if it's allowed to have unpaid execution.
		ensure!(T::contains(&actual_origin), ProcessMessageError::Unsupported);

		Ok(())
	}
}

/// Allows a message only if it is from a system-level child parachain.
pub struct IsChildSystemParachain<ParaId>(PhantomData<ParaId>);
impl<ParaId: IsSystem + From<u32>> Contains<Location> for IsChildSystemParachain<ParaId> {
	fn contains(l: &Location) -> bool {
		matches!(
			l.interior().as_slice(),
			[Junction::Parachain(id)]
				if ParaId::from(*id).is_system() && l.parent_count() == 0,
		)
	}
}

/// Matches if the given location is a system-level sibling parachain.
pub struct IsSiblingSystemParachain<ParaId, SelfParaId>(PhantomData<(ParaId, SelfParaId)>);
impl<ParaId: IsSystem + From<u32> + Eq, SelfParaId: Get<ParaId>> Contains<Location>
	for IsSiblingSystemParachain<ParaId, SelfParaId>
{
	fn contains(l: &Location) -> bool {
		matches!(
			l.unpack(),
			(1, [Junction::Parachain(id)])
				if SelfParaId::get() != ParaId::from(*id) && ParaId::from(*id).is_system(),
		)
	}
}

/// Matches if the given location contains only the specified amount of parents and no interior
/// junctions.
pub struct IsParentsOnly<Count>(PhantomData<Count>);
impl<Count: Get<u8>> Contains<Location> for IsParentsOnly<Count> {
	fn contains(t: &Location) -> bool {
		t.contains_parents_only(Count::get())
	}
}

/// Allows only messages if the generic `ResponseHandler` expects them via `expecting_response`.
pub struct AllowKnownQueryResponses<ResponseHandler>(PhantomData<ResponseHandler>);
impl<ResponseHandler: OnResponse> ShouldExecute for AllowKnownQueryResponses<ResponseHandler> {
	fn should_execute<RuntimeCall>(
		origin: &Location,
		instructions: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin, ?instructions, ?max_weight, ?properties,
			"AllowKnownQueryResponses"
		);
		instructions
			.matcher()
			.assert_remaining_insts(1)?
			.match_next_inst(|inst| match inst {
				QueryResponse { query_id, querier, .. }
					if ResponseHandler::expecting_response(origin, *query_id, querier.as_ref()) =>
				{
					Ok(())
				},
				_ => Err(ProcessMessageError::BadFormat),
			})?;
		Ok(())
	}
}

/// Allows execution from `origin` if it is just a straight `SubscribeVersion` or
/// `UnsubscribeVersion` instruction.
pub struct AllowSubscriptionsFrom<T>(PhantomData<T>);
impl<T: Contains<Location>> ShouldExecute for AllowSubscriptionsFrom<T> {
	fn should_execute<RuntimeCall>(
		origin: &Location,
		instructions: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin, ?instructions, ?max_weight, ?properties,
			"AllowSubscriptionsFrom",
		);
		ensure!(T::contains(origin), ProcessMessageError::Unsupported);
		instructions
			.matcher()
			.assert_remaining_insts(1)?
			.match_next_inst(|inst| match inst {
				SubscribeVersion { .. } | UnsubscribeVersion => Ok(()),
				_ => Err(ProcessMessageError::BadFormat),
			})?;
		Ok(())
	}
}

/// Allows execution for the Relay Chain origin (represented as `Location::parent()`) if it is just
/// a straight `HrmpNewChannelOpenRequest`, `HrmpChannelAccepted`, or `HrmpChannelClosing`
/// instruction.
///
/// Note: This barrier fulfills safety recommendations for the mentioned instructions - see their
/// documentation.
pub struct AllowHrmpNotificationsFromRelayChain;
impl ShouldExecute for AllowHrmpNotificationsFromRelayChain {
	fn should_execute<RuntimeCall>(
		origin: &Location,
		instructions: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		tracing::trace!(
			target: "xcm::barriers",
			?origin, ?instructions, ?max_weight, ?properties,
			"AllowHrmpNotificationsFromRelayChain"
		);
		// accept only the Relay Chain
		ensure!(matches!(origin.unpack(), (1, [])), ProcessMessageError::Unsupported);
		// accept only HRMP notifications and nothing else
		instructions
			.matcher()
			.assert_remaining_insts(1)?
			.match_next_inst(|inst| match inst {
				HrmpNewChannelOpenRequest { .. } |
				HrmpChannelAccepted { .. } |
				HrmpChannelClosing { .. } => Ok(()),
				_ => Err(ProcessMessageError::BadFormat),
			})?;
		Ok(())
	}
}

/// Deny executing the XCM if it matches any of the Deny filter regardless of anything else.
/// If it passes the Deny, and matches one of the Allow cases then it is let through.
pub struct DenyThenTry<Deny, Allow>(PhantomData<Deny>, PhantomData<Allow>)
where
	Deny: DenyExecution,
	Allow: ShouldExecute;

impl<Deny, Allow> ShouldExecute for DenyThenTry<Deny, Allow>
where
	Deny: DenyExecution,
	Allow: ShouldExecute,
{
	fn should_execute<RuntimeCall>(
		origin: &Location,
		message: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		Deny::deny_execution(origin, message, max_weight, properties)?;
		Allow::should_execute(origin, message, max_weight, properties)
	}
}

// See issue <https://github.com/paritytech/polkadot/issues/5233>
pub struct DenyReserveTransferToRelayChain;
impl DenyExecution for DenyReserveTransferToRelayChain {
	fn deny_execution<RuntimeCall>(
		origin: &Location,
		message: &mut [Instruction<RuntimeCall>],
		_max_weight: Weight,
		_properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		message.matcher().match_next_inst_while(
			|_| true,
			|inst| match inst {
				InitiateReserveWithdraw {
					reserve: Location { parents: 1, interior: Here },
					..
				} |
				DepositReserveAsset { dest: Location { parents: 1, interior: Here }, .. } |
				TransferReserveAsset { dest: Location { parents: 1, interior: Here }, .. } => {
					Err(ProcessMessageError::Unsupported) // Deny
				},

				// An unexpected reserve transfer has arrived from the Relay Chain. Generally,
				// `IsReserve` should not allow this, but we just log it here.
				ReserveAssetDeposited { .. }
					if matches!(origin, Location { parents: 1, interior: Here }) =>
				{
					tracing::debug!(
						target: "xcm::barriers",
						"Unexpected ReserveAssetDeposited from the Relay Chain",
					);
					Ok(ControlFlow::Continue(()))
				},

				_ => Ok(ControlFlow::Continue(())),
			},
		)?;
		Ok(())
	}
}

environmental::environmental!(recursion_count: u8);

/// Denies execution if the XCM contains instructions not meant to run on this chain,
/// first checking at the top-level and then **recursively**.
///
/// This barrier only applies to **locally executed** XCM instructions (`SetAppendix`,
/// `SetErrorHandler`, and `ExecuteWithOrigin`). Remote parts of the XCM are expected to be
/// validated by the receiving chain's barrier.
///
/// Note: Ensures that restricted instructions do not execute on the local chain, enforcing stricter
/// execution policies while allowing remote chains to enforce their own rules.
pub struct DenyRecursively<Inner>(PhantomData<Inner>);

impl<Inner: DenyExecution> DenyRecursively<Inner> {
	/// Recursively applies the deny filter to a nested XCM.
	///
	/// Ensures that restricted instructions are blocked at any depth within the XCM.
	/// Uses a **recursion counter** to prevent stack overflows from deep nesting.
	fn deny_recursively<RuntimeCall>(
		origin: &Location,
		xcm: &mut Xcm<RuntimeCall>,
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<ControlFlow<()>, ProcessMessageError> {
		// Initialise recursion counter for this execution context.
		recursion_count::using_once(&mut 1, || {
			// Prevent stack overflow by enforcing a recursion depth limit.
			recursion_count::with(|count| {
				if *count > xcm::RECURSION_LIMIT {
					tracing::debug!(
                    	target: "xcm::barriers",
                    	"Recursion limit exceeded (count: {count}), origin: {:?}, xcm: {:?}, max_weight: {:?}, properties: {:?}",
                    	origin, xcm, max_weight, properties
                	);
					return None;
				}
				*count = count.saturating_add(1);
				Some(())
			}).flatten().ok_or(ProcessMessageError::StackLimitReached)?;

			// Ensure the counter is decremented even if an early return occurs.
			sp_core::defer! {
				recursion_count::with(|count| {
					*count = count.saturating_sub(1);
				});
			}

			// Recursively check the nested XCM instructions.
			Self::deny_execution(origin, xcm.inner_mut(), max_weight, properties)
		})?;

		Ok(ControlFlow::Continue(()))
	}
}

impl<Inner: DenyExecution> DenyExecution for DenyRecursively<Inner> {
	/// Denies execution of restricted local nested XCM instructions.
	///
	/// This checks for `SetAppendix`, `SetErrorHandler`, and `ExecuteWithOrigin` instruction
	/// applying the deny filter **recursively** to any nested XCMs found.
	fn deny_execution<RuntimeCall>(
		origin: &Location,
		instructions: &mut [Instruction<RuntimeCall>],
		max_weight: Weight,
		properties: &mut Properties,
	) -> Result<(), ProcessMessageError> {
		// First, check if the top-level message should be denied.
		Inner::deny_execution(origin, instructions, max_weight, properties).inspect_err(|e| {
			tracing::debug!(
				target: "xcm::barriers",
				"DenyRecursively::Inner denied execution, origin: {:?}, instructions: {:?}, max_weight: {:?}, properties: {:?}, error: {:?}",
				origin, instructions, max_weight, properties, e
			);
		})?;

		// If the top-level check passes, check nested instructions recursively.
		instructions.matcher().match_next_inst_while(
			|_| true,
			|inst| match inst {
				SetAppendix(nested_xcm) |
				SetErrorHandler(nested_xcm) |
				ExecuteWithOrigin { xcm: nested_xcm, .. } => Self::deny_recursively::<RuntimeCall>(
					origin, nested_xcm, max_weight, properties,
				),
				_ => Ok(ControlFlow::Continue(())),
			},
		)?;

		// Permit everything else
		Ok(())
	}
}

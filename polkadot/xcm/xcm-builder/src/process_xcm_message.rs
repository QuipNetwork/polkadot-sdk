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

//! Implementation of `ProcessMessage` for an `ExecuteXcm` implementation.

use codec::{Decode, FullCodec, MaxEncodedLen};
use core::{fmt::Debug, marker::PhantomData};
use frame_support::{
	dispatch::GetDispatchInfo,
	traits::{ProcessMessage, ProcessMessageError},
};
use scale_info::TypeInfo;
use sp_weights::{Weight, WeightMeter};
use xcm::prelude::*;

const LOG_TARGET: &str = "xcm::process-message";

/// A message processor that delegates execution to an `XcmExecutor`.
pub struct ProcessXcmMessage<MessageOrigin, XcmExecutor, Call>(
	PhantomData<(MessageOrigin, XcmExecutor, Call)>,
);
impl<
		MessageOrigin: Into<Location> + FullCodec + MaxEncodedLen + Clone + Eq + PartialEq + TypeInfo + Debug,
		XcmExecutor: ExecuteXcm<Call>,
		Call: Decode + GetDispatchInfo,
	> ProcessMessage for ProcessXcmMessage<MessageOrigin, XcmExecutor, Call>
{
	type Origin = MessageOrigin;

	/// Process the given message, using no more than the remaining `weight` to do so.
	fn process_message(
		message: &[u8],
		origin: Self::Origin,
		meter: &mut WeightMeter,
		id: &mut XcmHash,
	) -> Result<bool, ProcessMessageError> {
		let versioned_message = VersionedXcm::<Call>::decode_all_with_mem_and_depth_limit(
			&mut &message[..],
		)
		.map_err(|e| {
			tracing::trace!(
				target: LOG_TARGET,
				?e,
				"`VersionedXcm` failed to decode",
			);

			ProcessMessageError::Corrupt
		})?;
		let message = Xcm::<Call>::try_from(versioned_message).map_err(|_| {
			tracing::trace!(
				target: LOG_TARGET,
				"Failed to convert `VersionedXcm` into `xcm::prelude::Xcm`!",
			);

			ProcessMessageError::Unsupported
		})?;
		let pre = XcmExecutor::prepare(message, Weight::MAX).map_err(|_| {
			tracing::trace!(
				target: LOG_TARGET,
				"Failed to prepare message.",
			);

			ProcessMessageError::Unsupported
		})?;
		// The worst-case weight:
		let required = pre.weight_of();
		if !meter.can_consume(required) {
			tracing::trace!(
				target: LOG_TARGET,
				"Xcm required {required} more than remaining {}",
				meter.remaining(),
			);

			return Err(ProcessMessageError::Overweight(required));
		}

		let (consumed, result) = match XcmExecutor::execute(origin.into(), pre, id, Weight::zero())
		{
			Outcome::Complete { used } => {
				tracing::trace!(
					target: LOG_TARGET,
					"XCM message execution complete, used weight: {used}",
				);
				(used, Ok(true))
			},
			Outcome::Incomplete { used, error: InstructionError { index, error } } => {
				tracing::trace!(
					target: LOG_TARGET,
					?error,
					?index,
					?used,
					"XCM message execution incomplete",
				);
				(used, Ok(false))
			},
			// In the error-case we assume the worst case and consume all possible weight.
			Outcome::Error(InstructionError { error, index }) => {
				tracing::trace!(
					target: LOG_TARGET,
					?error,
					?index,
					"XCM message execution error",
				);
				let error = match error {
					xcm::latest::Error::ExceedsStackLimit => ProcessMessageError::StackLimitReached,
					_ => ProcessMessageError::Unsupported,
				};

				(required, Err(error))
			},
		};
		meter.consume(consumed);
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::vec;
	use codec::Encode;
	use frame_support::{
		assert_err, assert_ok,
		traits::{ProcessMessageError, ProcessMessageError::*},
	};
	use polkadot_test_runtime::*;
	use xcm::{v3, v4, v5, VersionedXcm};

	const ORIGIN: Junction = Junction::OnlyChild;
	/// The processor to use for tests.
	type Processor =
		ProcessXcmMessage<Junction, xcm_executor::XcmExecutor<xcm_config::XcmConfig>, RuntimeCall>;

	#[test]
	fn process_message_trivial_works() {
		// ClearOrigin works.
		assert!(process(v3_xcm(true)).unwrap());
		assert!(process(v4_xcm(true)).unwrap());
		assert!(process(v5_xcm(true)).unwrap());
	}

	#[test]
	fn process_message_trivial_fails() {
		// Trap makes it fail.
		sp_io::TestExternalities::default().execute_with(|| {
			assert!(!process(v3_xcm(false)).unwrap());
			assert!(!process(v4_xcm(false)).unwrap());
			assert!(!process(v5_xcm(false)).unwrap());
		});
	}

	#[test]
	fn process_message_corrupted_fails() {
		let msgs: &[&[u8]] = &[&[], &[55, 66], &[123, 222, 233]];
		for msg in msgs {
			assert_err!(process_raw(msg), Corrupt);
		}
	}

	#[test]
	fn process_message_exceeds_limits_fails() {
		struct MockedExecutor;
		impl ExecuteXcm<()> for MockedExecutor {
			type Prepared = xcm_executor::WeighedMessage<()>;
			fn prepare(
				message: xcm::latest::Xcm<()>,
				_: Weight,
			) -> core::result::Result<Self::Prepared, InstructionError> {
				Ok(xcm_executor::WeighedMessage::new(Weight::zero(), message))
			}
			fn execute(
				_: impl Into<Location>,
				_: Self::Prepared,
				_: &mut XcmHash,
				_: Weight,
			) -> Outcome {
				Outcome::Error(InstructionError {
					index: 0,
					error: xcm::latest::Error::ExceedsStackLimit,
				})
			}
			fn charge_fees(_location: impl Into<Location>, _fees: Assets) -> xcm::latest::Result {
				unreachable!()
			}
		}

		type Processor = ProcessXcmMessage<Junction, MockedExecutor, ()>;

		let xcm = VersionedXcm::from(xcm::latest::Xcm::<()>(vec![
			xcm::latest::Instruction::<()>::ClearOrigin,
		]));
		assert_err!(
			Processor::process_message(
				&xcm.encode(),
				ORIGIN,
				&mut WeightMeter::new(),
				&mut [0; 32]
			),
			ProcessMessageError::StackLimitReached,
		);
	}

	#[test]
	fn process_message_overweight_fails() {
		sp_io::TestExternalities::default().execute_with(|| {
			for msg in [v4_xcm(true), v4_xcm(false), v4_xcm(false), v3_xcm(false)] {
				let msg = &msg.encode()[..];

				// Errors if we stay below a weight limit of 1000.
				for i in 0..10 {
					let meter = &mut WeightMeter::with_limit((i * 10).into());
					let mut id = [0; 32];
					assert_err!(
						Processor::process_message(msg, ORIGIN, meter, &mut id),
						Overweight(1000.into())
					);
					assert_eq!(meter.consumed(), 0.into());
				}

				// Works with a limit of 1000.
				let meter = &mut WeightMeter::with_limit(1000.into());
				let mut id = [0; 32];
				assert_ok!(Processor::process_message(msg, ORIGIN, meter, &mut id));
				assert_eq!(meter.consumed(), 1000.into());
			}
		});
	}

	fn v3_xcm(success: bool) -> VersionedXcm<RuntimeCall> {
		let instr = if success {
			v3::Instruction::<RuntimeCall>::ClearOrigin
		} else {
			v3::Instruction::<RuntimeCall>::Trap(1)
		};
		VersionedXcm::V3(v3::Xcm::<RuntimeCall>(vec![instr]))
	}

	fn v4_xcm(success: bool) -> VersionedXcm<RuntimeCall> {
		let instr = if success {
			v4::Instruction::<RuntimeCall>::ClearOrigin
		} else {
			v4::Instruction::<RuntimeCall>::Trap(1)
		};
		VersionedXcm::V4(v4::Xcm::<RuntimeCall>(vec![instr]))
	}

	fn v5_xcm(success: bool) -> VersionedXcm<RuntimeCall> {
		let instr = if success {
			v5::Instruction::<RuntimeCall>::ClearOrigin
		} else {
			v5::Instruction::<RuntimeCall>::Trap(1)
		};
		VersionedXcm::V5(v5::Xcm::<RuntimeCall>(vec![instr]))
	}

	fn process(msg: VersionedXcm<RuntimeCall>) -> Result<bool, ProcessMessageError> {
		process_raw(msg.encode().as_slice())
	}

	fn process_raw(raw: &[u8]) -> Result<bool, ProcessMessageError> {
		Processor::process_message(raw, ORIGIN, &mut WeightMeter::new(), &mut [0; 32])
	}
}

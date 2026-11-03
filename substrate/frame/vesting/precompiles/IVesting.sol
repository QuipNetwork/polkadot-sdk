// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

address constant VESTING_ADDR = 0x0000000000000000000000000000000000000902;

interface IVesting {
	/// Unlock any vested funds of the caller account.
	///
	/// The caller must have funds still locked under the vesting pallet.
	/// On success the vesting lock is reduced in line with the amount "vested" so far.
	///
	/// Reverts if the caller has no vesting schedule or if the origin is not signed.
	function vest() external;

	/// Unlock any vested funds of another account.
	///
	/// The `target` account must have funds still locked under the vesting pallet.
	/// On success the vesting lock is reduced in line with the amount "vested" so far.
	/// The caller pays the fee but the vesting schedule of `target` is updated.
	///
	/// Reverts if `target` has no vesting schedule or if the origin is not signed.
	function vestOther(address target) external;

	/// Returns the amount of funds still locked (to be vested) for the caller.
	///
	/// The returned value is in native (Substrate) denomination.
	/// Returns 0 in two cases: the caller has no vesting schedule, or the caller
	/// has a schedule but all funds are already unlocked (fully vested). Both cases
	/// mean there is nothing left to vest; calling vest() in either case will revert.
	function vestingBalance() external view returns (uint256);

	/// Returns the amount of funds still locked (to be vested) for `target`.
	///
	/// Identical semantics to vestingBalance() but queries an arbitrary account
	/// rather than the caller. Useful for contracts that need to pre-check whether
	/// vestOther(target) would do any work before dispatching it.
	///
	/// The returned value is in native (Substrate) denomination.
	/// Returns 0 if `target` has no vesting schedule or is fully vested.
	function vestingBalanceOf(address target) external view returns (uint256);

	/// Transfer funds from the caller to `target` with an attached vesting schedule.
	///
	/// The caller must have sufficient free balance to cover `locked`.
	/// A new vesting schedule is created for `target` that linearly unlocks
	/// `perBlock` tokens per block starting at `startingBlock`.
	///
	/// Reverts if `locked` is below the runtime's `MinVestedTransfer`, if
	/// `perBlock` is zero, or if `target` already has the maximum number of
	/// vesting schedules.
	function vestedTransfer(
		address target,
		uint256 locked,
		uint256 perBlock,
		uint256 startingBlock
	) external;
}

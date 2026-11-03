// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

address constant SYSTEM_ADDR = 0x0000000000000000000000000000000000000900;

interface ISystem {
	/// Computes the BLAKE2 256-bit hash on the given input.
	function hashBlake256(bytes memory input) external pure returns (bytes32 digest);

	/// Computes the BLAKE2 128-bit hash on the given input.
	function hashBlake128(bytes memory input) external pure returns (bytes32 digest);

	/// Retrieve the account id for a specified `H160` address.
	///
	/// Calling this function on a native `H160` chain (`type AccountId = H160`)
	/// does not make sense, as it would just return the `address` that it was
	/// called with.
	///
	/// # Note
	///
	/// If no mapping exists for `addr`, the fallback account id will be returned.
	function toAccountId(address input) external view returns (bytes memory account_id);

	/// Checks whether the caller of the contract calling this function is the origin
	/// of the whole call stack.
	function callerIsOrigin() external view returns (bool);

	/// Checks whether the caller of the contract calling this function is root.
	///
	/// Note that only the origin of the call stack can be root. Hence this
	/// function returning `true` implies that the contract is being called by the origin.
	///
	/// A return value of `true` indicates that this contract is being called by a root origin,
	/// and `false` indicates that the caller is a signed origin or another contract.
	function callerIsRoot() external view returns (bool);

	/// Checks whether the origin of the whole call stack is root.
	///
	/// Unlike `callerIsRoot`, this does not require the immediate caller to be the origin:
	/// it returns `true` whenever the top-level dispatch was made with a root origin, regardless
	/// of how many contract or delegate-call frames separate the precompile from that dispatch.
	/// This is the analogue of `tx.origin == ROOT`.
	///
	/// Contracts that need the stricter "my direct caller is root" check, including those
	/// behind an upgradeable proxy, must use `callerIsRoot`.
	function originIsRoot() external view returns (bool);

	/// Returns the minimum balance that is required for creating an account
	/// (the existential deposit).
	function minimumBalance() external view returns (uint);

	/// Returns the code hash of the caller.
	function ownCodeHash() external view returns (bytes32);

	/// Returns the amount of `Weight` left.
	function weightLeft() external view returns (uint64 refTime, uint64 proofSize);

	/// Terminate the calling contract of this function and send balance to `beneficiary`.
	/// This will revert if:
	/// - called from constructor
	/// - called from static context
	/// - called from delegate context
	function terminate(address beneficiary) external;

	/// Verify a sr25519 signature
	///
	/// # Parameters
	///
	/// - `signature`: The signature bytes.
	/// - `message`: The message bytes.
	/// - `publicKey`: The public key bytes.
	function sr25519Verify(uint8[64] calldata signature, bytes calldata message, bytes32 publicKey) external view returns (bool);

	/// Calculates the Ethereum address from the ECDSA compressed public key.
	/// This fails if ECDSA recovery of the provided key fails. 
	///
	/// # Parameters
	///
	/// - `publicKey`: The public key bytes.
	function ecdsaToEthAddress(uint8[33] calldata publicKey) external view returns (bytes20);
}

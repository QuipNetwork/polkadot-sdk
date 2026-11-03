// SPDX-License-Identifier: Apache-2.0
pragma solidity >=0.8.20;

interface IReentryStorage {
	function noop() external;
}

interface IReentryProxy {
	function bounce(address target) external;
}

/// Fixtures for the same-contract storage-deposit double-count regression
/// (contract-issues#213). A contract that writes storage, reenters itself
/// (directly or transitively), then writes storage again must persist the
/// exact same `ContractInfo` accounting as the equivalent non-reentrant run.
contract ReentryStorage {
	uint256 private s0;
	uint256 private s1;

	/// Baseline: two writes, no reentry.
	function writeTwice() external {
		s0 = 1;
		s1 = 1;
	}

	/// Write, reenter self (an empty frame), write. Same end state as `writeTwice`.
	function writeReenterWrite() external {
		s0 = 1;
		this.noop();
		s1 = 1;
	}

	/// Write, reenter self transitively through `proxy`, write. Same end state.
	function writeReenterWriteVia(address proxy) external {
		s0 = 1;
		IReentryProxy(proxy).bounce(address(this));
		s1 = 1;
	}

	/// The empty frame that gets reentered.
	function noop() external {}
}

/// Intermediary used to reach `ReentryStorage` transitively (X -> Y -> X).
contract ReentryProxy {
	function bounce(address target) external {
		IReentryStorage(target).noop();
	}
}

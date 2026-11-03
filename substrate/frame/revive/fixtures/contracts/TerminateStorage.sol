// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import "@revive/ISystem.sol";

/// Terminates itself through the system precompile, so the destruction is carried out at
/// the end of the call stack and works across transactions.
contract TerminateStorageInner {
	constructor() payable {}

	receive() external payable {}

	function terminateSelf(address beneficiary) external {
		bytes memory data = abi.encodeWithSelector(ISystem.terminate.selector, beneficiary);
		(bool success, bytes memory returnData) = SYSTEM_ADDR.call(data);
		if (!success) {
			assembly {
				revert(add(returnData, 0x20), mload(returnData))
			}
		}
	}
}

/// Writes `n` fresh storage slots into its own trie, optionally driving an inner contract
/// through a schedule-terminate, then-fund, then-write sequence.
contract TerminateStorageCaller {
	constructor() payable {}

	function writeOnly(uint64 n) external {
		writeFresh(n);
	}

	function writeAndTerminate(address payable inner, uint64 n, address beneficiary) external {
		TerminateStorageInner(inner).terminateSelf(beneficiary);
		(bool ok, ) = inner.call{value: 1_000_000_000_000}("");
		require(ok, "funding inner failed");
		writeFresh(n);
	}

	function writeFresh(uint64 n) private {
		for (uint64 i = 0; i < n; i++) {
			uint256 slot = uint256(i) + 1;
			uint256 value = uint256(i) + 1;
			assembly {
				sstore(slot, value)
			}
		}
	}
}

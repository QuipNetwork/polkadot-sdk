// SPDX-License-Identifier: MIT

pragma solidity >=0.8.4;

contract ContractRequiringBinarySearchForGasEstimation {
	function main() public view {
		this.expensive_operation{gas: gasleft() / 2}();
	}

	function expensive_operation() external pure returns (uint256 sum) {
		for (uint256 i = 0; i < 500; i++) {
			sum += i * i;
		}
	}
}

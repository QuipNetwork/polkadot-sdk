// SPDX-License-Identifier: MIT

pragma solidity >=0.4.21;

contract ContractWithConsumeAllGas {
	function test() external {
		assembly {
			mstore(0, 0xcc572cf9) // main selector
			mstore(32, 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF)
			mstore(64, 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF)
			let gas_value := div(mul(gas(), 1), 100)
			let success := call(gas_value, address(), 0, 28, 68, 0, 0)

			mstore(0, success)
			return(0, 32)
		}
	}

	function main(uint256 offset, uint256 len) external pure {
		assembly {
			// nullify memory ptr slot
			mstore(0x40, 0)
			revert(offset, len)
		}
	}
}

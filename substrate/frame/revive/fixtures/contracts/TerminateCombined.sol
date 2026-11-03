// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import "@revive/ISystem.sol";

/// Terminates itself through the pre-compile or `SELFDESTRUCT`, also several times in one call or
/// combined with a payout.
contract TerminateCombined {
	uint8 public constant METHOD_PRECOMPILE = 0;
	uint8 public constant METHOD_DELEGATE_CALL = 1;
	uint8 public constant METHOD_SYSCALL = 2;

	receive() external payable {}

	constructor(bool skip, uint8 method, address beneficiary) payable {
		if (skip) {
			return;
		}
		_terminate(method, beneficiary);
	}

	function echo(uint value) external pure returns (uint) {
		return value;
	}

	function terminate(uint8 method, address beneficiary) external {
		_terminate(method, beneficiary);
	}

	/// Pay out the whole balance to `to`, then terminate through the pre-compile.
	function payoutAndTerminate(address payable to, address beneficiary) external {
		(bool ok, ) = to.call{value: address(this).balance}("");
		require(ok, "payout failed");
		_terminate(METHOD_PRECOMPILE, beneficiary);
	}

	/// Terminate through the pre-compile twice in the same call.
	function terminateTwice(address beneficiary) external {
		_terminate(METHOD_PRECOMPILE, beneficiary);
		_terminate(METHOD_PRECOMPILE, beneficiary);
	}

	/// Terminate through the pre-compile, then run `SELFDESTRUCT` in this frame or in a delegate
	/// call to this contract.
	function terminateThenSelfdestruct(address beneficiary, bool viaDelegateCall) external {
		_terminate(METHOD_PRECOMPILE, beneficiary);
		if (!viaDelegateCall) {
			// Halts execution.
			_terminate(METHOD_SYSCALL, beneficiary);
		}
		bytes memory data = abi.encodeWithSelector(this.terminate.selector, METHOD_SYSCALL, beneficiary);
		(bool success, bytes memory returnData) = address(this).delegatecall(data);
		if (!success) {
			assembly {
				revert(add(returnData, 0x20), mload(returnData))
			}
		}
	}

	/// Run `SELFDESTRUCT` in a delegate call to this contract, then terminate through the
	/// pre-compile.
	function selfdestructThenTerminate(address beneficiary) external {
		bytes memory data = abi.encodeWithSelector(this.terminate.selector, METHOD_SYSCALL, beneficiary);
		(bool success, bytes memory returnData) = address(this).delegatecall(data);
		if (!success) {
			assembly {
				revert(add(returnData, 0x20), mload(returnData))
			}
		}
		_terminate(METHOD_PRECOMPILE, beneficiary);
	}

	/// Call terminate and forward any revert.
	/// Internal dispatcher: executes termination by
	/// - delegatecall (METHOD_DELEGATE_CALL) to system precompile
	/// - direct call (METHOD_PRECOMPILE) to system precompile
	/// - selfdestruct (METHOD_SYSCALL) sending balance to beneficiary
	function _terminate(uint8 method, address beneficiary) private {
		bytes memory data = abi.encodeWithSelector(ISystem.terminate.selector, beneficiary);
		(bool success, bytes memory returnData) = (false, "");

		if (method == METHOD_DELEGATE_CALL) {
			(success, returnData) = SYSTEM_ADDR.delegatecall(data);
		} else if (method == METHOD_PRECOMPILE) {
			(success, returnData) = SYSTEM_ADDR.call(data);
		} else if (method == METHOD_SYSCALL) {
			assembly {
				selfdestruct(beneficiary)
			}
			// selfdestruct halts execution, so if we reach here, something went wrong.
			revert("selfdestruct opcode returned");
		} else {
			revert("Invalid TerminateMethod");
		}

		if (!success) {
			assembly {
				revert(add(returnData, 0x20), mload(returnData))
			}
		}
	}
}

/// Creates `TerminateCombined` contracts and terminates them, or terminates an existing one twice.
contract TerminateCombinedCaller {
	TerminateCombined inner;

	receive() external payable {}

	constructor() payable {}

	function createAndTerminate(uint value, uint8 method, address beneficiary) external returns (address) {
		inner = new TerminateCombined{value: value}(true, method, beneficiary);
		inner.terminate(method, beneficiary);
		return address(inner);
	}

	function createAndTerminateThenSelfdestruct(uint value, address beneficiary, bool viaDelegateCall) external returns (address) {
		inner = new TerminateCombined{value: value}(true, 0, beneficiary);
		inner.terminateThenSelfdestruct(beneficiary, viaDelegateCall);
		return address(inner);
	}

	function createAndSelfdestructInConstructor(uint value, address beneficiary) external returns (address) {
		// 2 is `TerminateCombined.METHOD_SYSCALL`: the constructor runs `SELFDESTRUCT`.
		return address(new TerminateCombined{value: value}(false, 2, beneficiary));
	}

	function terminateTwiceAndSendFunds(address payable terminate_addr, uint value, uint8 method1, address beneficiary1, uint8 method2, address beneficiary2) external {
		TerminateCombined(terminate_addr).terminate(method1, beneficiary1);
		TerminateCombined(terminate_addr).terminate(method2, beneficiary2);
		(bool success, ) = terminate_addr.call{value: value}("");
		require(success, "sending funds failed");
	}
}

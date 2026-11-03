// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import "@revive/ISystem.sol";

contract NestedDeployer {
    function deployChild() external returns (address) {
        return address(new NestedChild());
    }

    /// Create and immediately destroy the child in the same tx — exercises the
    /// SELFDESTRUCT path (`only_if_same_tx: true`, EIP-6780 compliant).
    function deployAndDestroyChild(address payable beneficiary) external returns (address) {
        NestedChild child = new NestedChild();
        address childAddr = address(child);
        child.destroy(beneficiary);
        return childAddr;
    }
}

contract NestedChild {
    uint256 public state;

    constructor() {
        state = 42;
    }

    /// Self-terminate via the SELFDESTRUCT opcode (`only_if_same_tx: true`,
    /// EIP-6780): only actually destroys the contract if invoked in the same tx
    /// that created it.
    function destroy(address payable beneficiary) external {
        selfdestruct(beneficiary);
    }

    /// Self-terminate via the system precompile (`only_if_same_tx: false`), so the
    /// contract can be destroyed in a later tx than the one that created it.
    function destroyViaPrecompile(address beneficiary) external {
        bytes memory data = abi.encodeWithSelector(ISystem.terminate.selector, beneficiary);
        (bool success, bytes memory returnData) = SYSTEM_ADDR.call(data);
        if (!success) {
            assembly {
                revert(add(returnData, 0x20), mload(returnData))
            }
        }
    }
}

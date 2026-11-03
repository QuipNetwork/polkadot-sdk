// SPDX-License-Identifier: MIT
pragma solidity ^0.8.4;

/// Minimal two-slot storage contract for testing storage overrides.
/// Each variable is declared as `uint256` to guarantee separate storage slots
/// (slot 0 for `first`, slot 1 for `second`), while the constructor accepts
/// `uint64` to keep the ABI lightweight.
contract TwoSlots {
    uint256 public first;
    uint256 public second;

    constructor(uint64 _first, uint64 _second) {
        first = _first;
        second = _second;
    }
}

// SPDX-License-Identifier: MIT
pragma solidity ^0.8.4;

/// Emits multiple events in a single call for testing log collection.
contract MultiEvent {
    event Ping(uint64 value);
    event Pong(uint64 value);

    function emitMultiple(uint64 a, uint64 b) public {
        emit Ping(a);
        emit Pong(b);
    }
}

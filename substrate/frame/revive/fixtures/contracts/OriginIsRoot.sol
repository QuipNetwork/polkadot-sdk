// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@revive/ISystem.sol";

/// Exercises the `System.originIsRoot`, `System.callerIsRoot` and `System.callerIsOrigin`
/// precompile methods through various call shapes.
///
/// A single instance can play either role: the contract that ultimately invokes the
/// precompile, or a proxy that reaches another instance through a regular call or
/// delegate call.
contract OriginIsRoot {
	/// Directly invoke `originIsRoot()` on the System precompile.
	function originIsRoot() external view returns (bool) {
		return ISystem(SYSTEM_ADDR).originIsRoot();
	}

	/// Directly invoke `callerIsRoot()` on the System precompile.
	function callerIsRoot() external view returns (bool) {
		return ISystem(SYSTEM_ADDR).callerIsRoot();
	}

	/// Directly invoke `callerIsOrigin()` on the System precompile.
	function callerIsOrigin() external view returns (bool) {
		return ISystem(SYSTEM_ADDR).callerIsOrigin();
	}

	/// Regular contract call into `target.originIsRoot()`.
	function callOriginIsRoot(address target) external view returns (bool) {
		return OriginIsRoot(target).originIsRoot();
	}

	/// Regular contract call into `target.callerIsRoot()`.
	function callCallerIsRoot(address target) external view returns (bool) {
		return OriginIsRoot(target).callerIsRoot();
	}

	/// Regular contract call into `target.callerIsOrigin()`.
	function callCallerIsOrigin(address target) external view returns (bool) {
		return OriginIsRoot(target).callerIsOrigin();
	}

	/// Regular call to `target` with arbitrary `data`, decoding the returned `bool`. `target`
	/// sees this contract as its caller, so passing the calldata of a `delegate*` function puts
	/// a regular call in front of a delegate call.
	function forward(address target, bytes calldata data) external returns (bool) {
		(bool ok, bytes memory ret) = target.call(data);
		require(ok, "forward failed");
		return abi.decode(ret, (bool));
	}

	/// Delegate-call into `impl.originIsRoot()`, the same shape as an upgradeable proxy
	/// dispatching into its implementation.
	function delegateOriginIsRoot(address _impl) external returns (bool) {
		(bool ok, bytes memory ret) =
			_impl.delegatecall(abi.encodeWithSelector(this.originIsRoot.selector));
		require(ok, "delegate originIsRoot failed");
		return abi.decode(ret, (bool));
	}

	/// Delegate-call into `impl.callerIsRoot()`, the same shape as an upgradeable proxy
	/// dispatching into its implementation.
	function delegateCallerIsRoot(address _impl) external returns (bool) {
		(bool ok, bytes memory ret) =
			_impl.delegatecall(abi.encodeWithSelector(this.callerIsRoot.selector));
		require(ok, "delegate callerIsRoot failed");
		return abi.decode(ret, (bool));
	}

	/// Delegate-call into `impl.callerIsOrigin()`, the same shape as an upgradeable proxy
	/// dispatching into its implementation.
	function delegateCallerIsOrigin(address _impl) external returns (bool) {
		(bool ok, bytes memory ret) =
			_impl.delegatecall(abi.encodeWithSelector(this.callerIsOrigin.selector));
		require(ok, "delegate callerIsOrigin failed");
		return abi.decode(ret, (bool));
	}

	/// Delegate-call `_impl` with arbitrary `data`, decoding the returned `bool`. The delegated
	/// code runs as this contract: the calldata of a `call*` function makes it call `target` with
	/// a regular call, so `target` sees this contract as its caller, and the calldata of a
	/// `delegate*` function chains a second delegate call.
	function forwardDelegate(address _impl, bytes calldata data) external returns (bool) {
		(bool ok, bytes memory ret) = _impl.delegatecall(data);
		require(ok, "forward delegate failed");
		return abi.decode(ret, (bool));
	}
}

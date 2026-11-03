// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use super::{code_provider::CodeProvider, ClientConfig};
use sc_client_api::{
	backend, call_executor::CallExecutor, execution_extensions::ExecutionExtensions, HeaderBackend,
	TrieCacheContext,
};
use sc_executor::{RuntimeVersion, RuntimeVersionOf};
use sp_api::ProofRecorder;
use sp_core::traits::{CallContext, CodeExecutor};
use sp_externalities::Extensions;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, HashingFor},
};
use sp_state_machine::{
	backend::{AsTrieBackend, TryPendingCode},
	OverlayedChanges, StateMachine, StorageProof,
};
use std::{cell::RefCell, sync::Arc};

/// Call executor that executes methods locally, querying all required
/// data from local backend.
pub struct LocalCallExecutor<Block: BlockT, B, E> {
	backend: Arc<B>,
	executor: E,
	code_provider: CodeProvider<Block, B, E>,
	execution_extensions: Arc<ExecutionExtensions<Block>>,
}

impl<Block: BlockT, B, E> LocalCallExecutor<Block, B, E>
where
	E: CodeExecutor + RuntimeVersionOf + Clone + 'static,
	B: backend::Backend<Block>,
{
	/// Creates new instance of local call executor.
	pub fn new(
		backend: Arc<B>,
		executor: E,
		client_config: ClientConfig<Block>,
		execution_extensions: ExecutionExtensions<Block>,
	) -> sp_blockchain::Result<Self> {
		let code_provider = CodeProvider::new(&client_config, executor.clone(), backend.clone())?;

		Ok(LocalCallExecutor {
			backend,
			executor,
			code_provider,
			execution_extensions: Arc::new(execution_extensions),
		})
	}
}

impl<Block: BlockT, B, E> Clone for LocalCallExecutor<Block, B, E>
where
	E: Clone,
{
	fn clone(&self) -> Self {
		LocalCallExecutor {
			backend: self.backend.clone(),
			executor: self.executor.clone(),
			code_provider: self.code_provider.clone(),
			execution_extensions: self.execution_extensions.clone(),
		}
	}
}

impl<B, E, Block> CallExecutor<Block> for LocalCallExecutor<Block, B, E>
where
	B: backend::Backend<Block>,
	E: CodeExecutor + RuntimeVersionOf + Clone + 'static,
	Block: BlockT,
{
	type Error = E::Error;

	type Backend = B;

	fn execution_extensions(&self) -> &ExecutionExtensions<Block> {
		&self.execution_extensions
	}

	fn call(
		&self,
		at_hash: Block::Hash,
		method: &str,
		call_data: &[u8],
		context: CallContext,
	) -> sp_blockchain::Result<Vec<u8>> {
		let mut changes = OverlayedChanges::default();
		let at_number =
			self.backend.blockchain().expect_block_number_from_id(&BlockId::Hash(at_hash))?;
		let state = self.backend.state_at(at_hash, context.into())?;

		let state_runtime_code =
			sp_state_machine::backend::BackendRuntimeCode::new(&state, context.into());
		let runtime_code =
			state_runtime_code.runtime_code().map_err(sp_blockchain::Error::RuntimeCode)?;

		let runtime_code = self.code_provider.maybe_override_code(runtime_code, &state, at_hash)?.0;

		let mut extensions = self.execution_extensions.extensions(at_hash, at_number);

		let mut sm = StateMachine::new(
			&state,
			&mut changes,
			&self.executor,
			method,
			call_data,
			&mut extensions,
			&runtime_code,
			context,
		)
		.set_parent_hash(at_hash);

		sm.execute().map_err(Into::into)
	}

	fn contextual_call(
		&self,
		at_hash: Block::Hash,
		method: &str,
		call_data: &[u8],
		changes: &RefCell<OverlayedChanges<HashingFor<Block>>>,
		recorder: &Option<ProofRecorder<Block>>,
		call_context: CallContext,
		extensions: &RefCell<Extensions>,
	) -> Result<Vec<u8>, sp_blockchain::Error> {
		let state = self.backend.state_at(at_hash, call_context.into())?;

		let changes = &mut *changes.borrow_mut();

		// It is important to extract the runtime code here before we create the proof
		// recorder to not record it. We also need to fetch the runtime code from `state` to
		// make sure we use the caching layers.
		let state_runtime_code =
			sp_state_machine::backend::BackendRuntimeCode::new(&state, call_context.into());

		let runtime_code =
			state_runtime_code.runtime_code().map_err(sp_blockchain::Error::RuntimeCode)?;
		let runtime_code = self.code_provider.maybe_override_code(runtime_code, &state, at_hash)?.0;
		let mut extensions = extensions.borrow_mut();

		match recorder {
			Some(recorder) => {
				let trie_state = state.as_trie_backend();

				let backend = sp_state_machine::TrieBackendBuilder::wrap(&trie_state)
					.with_recorder(recorder.clone())
					.build();

				let mut state_machine = StateMachine::new(
					&backend,
					changes,
					&self.executor,
					method,
					call_data,
					&mut extensions,
					&runtime_code,
					call_context,
				)
				.set_parent_hash(at_hash);
				state_machine.execute()
			},
			None => {
				let mut state_machine = StateMachine::new(
					&state,
					changes,
					&self.executor,
					method,
					call_data,
					&mut extensions,
					&runtime_code,
					call_context,
				)
				.set_parent_hash(at_hash);
				state_machine.execute()
			},
		}
		.map_err(Into::into)
	}

	fn runtime_version(
		&self,
		at_hash: Block::Hash,
		call_context: CallContext,
	) -> sp_blockchain::Result<RuntimeVersion> {
		let state = self.backend.state_at(at_hash, backend::TrieCacheContext::Untrusted)?;
		let state_runtime_code =
			sp_state_machine::backend::BackendRuntimeCode::new(&state, call_context.into());

		let runtime_code =
			state_runtime_code.runtime_code().map_err(sp_blockchain::Error::RuntimeCode)?;
		self.code_provider
			.maybe_override_code(runtime_code, &state, at_hash)
			.map(|(_, v)| v)
	}

	fn prove_execution(
		&self,
		at_hash: Block::Hash,
		method: &str,
		call_data: &[u8],
	) -> sp_blockchain::Result<(Vec<u8>, StorageProof)> {
		let at_number =
			self.backend.blockchain().expect_block_number_from_id(&BlockId::Hash(at_hash))?;
		let state = self.backend.state_at(at_hash, TrieCacheContext::Untrusted)?;

		let trie_backend = state.as_trie_backend();

		let state_runtime_code =
			sp_state_machine::backend::BackendRuntimeCode::new(trie_backend, TryPendingCode::No);
		let runtime_code =
			state_runtime_code.runtime_code().map_err(sp_blockchain::Error::RuntimeCode)?;
		let runtime_code = self.code_provider.maybe_override_code(runtime_code, &state, at_hash)?.0;

		sp_state_machine::prove_execution_on_trie_backend(
			trie_backend,
			&mut Default::default(),
			&self.executor,
			method,
			call_data,
			&runtime_code,
			&mut self.execution_extensions.extensions(at_hash, at_number),
		)
		.map_err(Into::into)
	}
}

impl<B, E, Block> RuntimeVersionOf for LocalCallExecutor<Block, B, E>
where
	E: RuntimeVersionOf,
	Block: BlockT,
{
	fn runtime_version(
		&self,
		ext: &mut dyn sp_externalities::Externalities,
		runtime_code: &sp_core::traits::RuntimeCode,
	) -> Result<sp_version::RuntimeVersion, sc_executor::error::Error> {
		RuntimeVersionOf::runtime_version(&self.executor, ext, runtime_code)
	}
}

impl<Block, B, E> sp_version::GetRuntimeVersionAt<Block> for LocalCallExecutor<Block, B, E>
where
	B: backend::Backend<Block>,
	E: CodeExecutor + RuntimeVersionOf + Clone + 'static,
	Block: BlockT,
{
	fn runtime_version(
		&self,
		at: Block::Hash,
		call_context: CallContext,
	) -> Result<sp_version::RuntimeVersion, String> {
		CallExecutor::runtime_version(self, at, call_context).map_err(|e| e.to_string())
	}
}

// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use alloc::{
	collections::BTreeMap,
	string::{String, ToString},
};
use codec::{Decode, Encode};
use core::iter::IntoIterator;
use scale_info::TypeInfo;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, TypeInfo, Default)]
pub struct ReviveRuntimeApiVersionDeclarations(BTreeMap<String, u8>);

impl ReviveRuntimeApiVersionDeclarations {
	pub fn new() -> Self {
		Default::default()
	}

	pub fn insert(mut self, runtime_api_function: impl ToString, value: u8) -> Self {
		self.0.insert(runtime_api_function.to_string(), value);
		self
	}

	pub fn get(&self, runtime_api_function: impl AsRef<str>) -> Option<u8> {
		self.0.get(runtime_api_function.as_ref()).copied()
	}

	pub fn iter(&self) -> impl Iterator<Item = (&str, &u8)> {
		self.0.iter().map(|(key, value)| (key.as_str(), value))
	}
}

impl IntoIterator for ReviveRuntimeApiVersionDeclarations {
	type Item = <BTreeMap<String, u8> as IntoIterator>::Item;
	type IntoIter = <BTreeMap<String, u8> as IntoIterator>::IntoIter;

	fn into_iter(self) -> Self::IntoIter {
		IntoIterator::into_iter(self.0)
	}
}

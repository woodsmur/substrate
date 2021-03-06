// Copyright 2017 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Proving state machine backend.

use std::cell::RefCell;
use hashdb::{Hasher, HashDB};
use memorydb::MemoryDB;
use patricia_trie::{TrieDB, Trie, Recorder, NodeCodec};
use trie_backend::{TrieBackend, Ephemeral};
use {Error, ExecutionError, Backend, TryIntoTrieBackend};
use rlp::Encodable;
use heapsize::HeapSizeOf;

/// Patricia trie-based backend which also tracks all touched storage trie values.
/// These can be sent to remote node and used as a proof of execution.
pub struct ProvingBackend<H: Hasher, C: NodeCodec<H>> {
	backend: TrieBackend<H, C>,
	proof_recorder: RefCell<Recorder<H::Out>>,
}

impl<H: Hasher, C: NodeCodec<H>> ProvingBackend<H, C> {
	/// Create new proving backend.
	pub fn new(backend: TrieBackend<H, C>) -> Self {
		ProvingBackend {
			backend,
			proof_recorder: RefCell::new(Recorder::new()),
		}
	}

	/// Consume the backend, extracting the gathered proof in lexicographical order
	/// by value.
	pub fn extract_proof(self) -> Vec<Vec<u8>> {
		self.proof_recorder.into_inner().drain()
			.into_iter()
			.map(|n| n.data.to_vec())
			.collect()
	}
}

impl<H, C> Backend<H, C> for ProvingBackend<H, C>
where
	H: Hasher,
	C: NodeCodec<H>,
	H::Out: Ord + Encodable + HeapSizeOf
{
	type Error = String;
	type Transaction = MemoryDB<H>;

	fn storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
		let mut read_overlay = MemoryDB::new();
		let eph = Ephemeral::new(
			self.backend.backend_storage(),
			&mut read_overlay,
		);
		let map_e = |e| format!("Trie lookup error: {}", e);

		let mut proof_recorder = self.proof_recorder.try_borrow_mut()
			.expect("only fails when already borrowed; storage() is non-reentrant; qed");
		TrieDB::<H, C>::new(&eph, &self.backend.root()).map_err(map_e)?
			.get_with(key, &mut *proof_recorder).map(|x| x.map(|val| val.to_vec())).map_err(map_e)
	}

	fn for_keys_with_prefix<F: FnMut(&[u8])>(&self, prefix: &[u8], f: F) {
		self.backend.for_keys_with_prefix(prefix, f)
	}

	fn pairs(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
		self.backend.pairs()
	}

	fn storage_root<I>(&self, delta: I) -> (H::Out, MemoryDB<H>)
		where I: IntoIterator<Item=(Vec<u8>, Option<Vec<u8>>)>
	{
		self.backend.storage_root(delta)
	}
}

impl<H: Hasher, C: NodeCodec<H>> TryIntoTrieBackend<H, C> for ProvingBackend<H, C> {
	fn try_into_trie_backend(self) -> Option<TrieBackend<H, C>> {
		None
	}
}

/// Create proof check backend.
pub fn create_proof_check_backend<H, C>(
	root: H::Out,
	proof: Vec<Vec<u8>>
) -> Result<TrieBackend<H, C>, Box<Error>>
where
	H: Hasher,
	C: NodeCodec<H>,
	H::Out: HeapSizeOf,
{
	let mut db = MemoryDB::new();
	for item in proof {
		db.insert(&item);
	}

	if !db.contains(&root) {
		return Err(Box::new(ExecutionError::InvalidProof) as Box<Error>);
	}


	Ok(TrieBackend::with_memorydb(db, root))
}

#[cfg(test)]
mod tests {
	use backend::{InMemory};
	use trie_backend::tests::test_trie;
	use super::*;
	use primitives::{KeccakHasher, RlpCodec};

	fn test_proving() -> ProvingBackend<KeccakHasher, RlpCodec> {
		ProvingBackend::new(test_trie())
	}

	#[test]
	fn proof_is_empty_until_value_is_read() {
		assert!(test_proving().extract_proof().is_empty());
	}

	#[test]
	fn proof_is_non_empty_after_value_is_read() {
		let backend = test_proving();
		assert_eq!(backend.storage(b"key").unwrap(), Some(b"value".to_vec()));
		assert!(!backend.extract_proof().is_empty());
	}

	#[test]
	fn proof_is_invalid_when_does_not_contains_root() {
		assert!(create_proof_check_backend::<KeccakHasher, RlpCodec>(1.into(), vec![]).is_err());
	}

	#[test]
	fn passes_throgh_backend_calls() {
		let trie_backend = test_trie();
		let proving_backend = test_proving();
		assert_eq!(trie_backend.storage(b"key").unwrap(), proving_backend.storage(b"key").unwrap());
		assert_eq!(trie_backend.pairs(), proving_backend.pairs());

		let (trie_root, mut trie_mdb) = trie_backend.storage_root(::std::iter::empty());
		let (proving_root, mut proving_mdb) = proving_backend.storage_root(::std::iter::empty());
		assert_eq!(trie_root, proving_root);
		assert_eq!(trie_mdb.drain(), proving_mdb.drain());
	}

	#[test]
	fn proof_recorded_and_checked() {
		let contents = (0..64).map(|i| (vec![i], Some(vec![i]))).collect::<Vec<_>>();
		let in_memory = InMemory::<KeccakHasher, RlpCodec>::default();
		let in_memory = in_memory.update(contents);
		let in_memory_root = in_memory.storage_root(::std::iter::empty()).0;
		(0..64).for_each(|i| assert_eq!(in_memory.storage(&[i]).unwrap().unwrap(), vec![i]));

		let trie = in_memory.try_into_trie_backend().unwrap();
		let trie_root = trie.storage_root(::std::iter::empty()).0;
		assert_eq!(in_memory_root, trie_root);
		(0..64).for_each(|i| assert_eq!(trie.storage(&[i]).unwrap().unwrap(), vec![i]));

		let proving = ProvingBackend::new(trie);
		assert_eq!(proving.storage(&[42]).unwrap().unwrap(), vec![42]);

		let proof = proving.extract_proof();

		let proof_check = create_proof_check_backend::<KeccakHasher, RlpCodec>(in_memory_root.into(), proof).unwrap();
		assert_eq!(proof_check.storage(&[42]).unwrap().unwrap(), vec![42]);
	}
}

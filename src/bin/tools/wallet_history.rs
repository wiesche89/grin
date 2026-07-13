// Copyright 2026 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use chrono::Utc;
use grin_chain::store::ChainStore;
use grin_core::consensus::{valid_header_version, WEEK_HEIGHT};
use grin_core::core::hash::Hash;
use grin_core::core::{BlockHeader, CommitWrapper, HeaderVersion};
use grin_core::global;
use grin_core::libtx::proof::{self, LegacyProofBuilder, ProofBuilder};
use grin_keychain::{mnemonic, ExtKeychain, Identifier, Keychain, SwitchCommitmentType, ViewKey};
use grin_util::ToHex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufWriter, Write};

#[derive(Debug)]
pub struct ScanSummary {
	pub outputs: usize,
	pub unspent_outputs: usize,
	pub final_balance: u64,
}

#[derive(Clone, Debug, Serialize)]
struct HistoricalOutput {
	commitment: String,
	amount: u64,
	key_id: String,
	switch_type: String,
	is_coinbase: bool,
	created_height: u64,
	created_at: String,
	spendable_height: u64,
	spent_height: Option<u64>,
	spent_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct BalancePoint {
	height: u64,
	timestamp: String,
	received: u64,
	spent: u64,
	balance: u64,
}

#[derive(Debug, Serialize)]
struct HistoryReport {
	chain_height: u64,
	generated_at: String,
	final_balance: u64,
	outputs: Vec<HistoricalOutput>,
	balance_timeline: Vec<BalancePoint>,
}

#[derive(Default)]
struct BalanceChange {
	timestamp: String,
	received: u64,
	spent: u64,
}

pub fn scan_wallet_history(db_root: &str, output_path: &str) -> Result<ScanSummary, String> {
	let store = ChainStore::new(db_root, None)
		.map_err(|e| format!("unable to open archive chain database: {}", e))?;
	let head = validate_archive_store(&store)?;

	let words = read_visible_line("Wallet seed (12 or 24 words, visible): ")?;
	let keychain = keychain_from_mnemonic(&words)?;
	drop(words);
	let rewind_hash = ViewKey::rewind_hash(keychain.secp(), keychain.public_root_key()).to_hex();
	println!("Wallet rewind hash: {}", rewind_hash);

	println!("Archive chain head: {}", head.height);
	println!("Pass 1/2: scanning historical outputs...");
	let mut outputs = find_owned_outputs(&store, head.last_block_h, head.height, &keychain)?;
	println!("Pass 1/2 complete: {} wallet outputs found.", outputs.len());
	println!("Pass 2/2: locating spends...");
	find_spends(&store, head.last_block_h, head.height, &mut outputs)?;
	println!("Pass 2/2 complete.");
	outputs.sort_by_key(|o| (o.created_height, o.commitment.clone()));

	let balance_timeline = build_balance_timeline(&outputs)?;
	let final_balance = balance_timeline.last().map(|p| p.balance).unwrap_or(0);
	let unspent_outputs = outputs.iter().filter(|o| o.spent_height.is_none()).count();
	let report = HistoryReport {
		chain_height: head.height,
		generated_at: Utc::now().to_rfc3339(),
		final_balance,
		outputs,
		balance_timeline,
	};
	let output = File::create(output_path)
		.map_err(|e| format!("unable to create {}: {}", output_path, e))?;
	serde_json::to_writer_pretty(BufWriter::new(output), &report)
		.map_err(|e| format!("unable to write history: {}", e))?;

	Ok(ScanSummary {
		outputs: report.outputs.len(),
		unspent_outputs,
		final_balance,
	})
}

fn validate_archive_store(store: &ChainStore) -> Result<grin_chain::types::Tip, String> {
	let head = store
		.head()
		.map_err(|e| format!("unable to read chain head: {}", e))?;
	let tail = store
		.tail()
		.map_err(|e| format!("unable to read chain tail: {}", e))?;
	if tail.height > 1 {
		return Err(format!(
			"chain data is pruned: earliest retained full block is at height {}",
			tail.height
		));
	}
	let tail_header = store.get_block_header(&tail.last_block_h).map_err(|e| {
		format!(
			"chain data is incomplete: tail header {} is missing: {}",
			tail.last_block_h, e
		)
	})?;
	store.get_block(&tail.last_block_h).map_err(|e| {
		format!(
			"chain data is incomplete: tail full block {} is missing: {}",
			tail.last_block_h, e
		)
	})?;
	let genesis_hash = if tail.height == 0 {
		tail.last_block_h
	} else {
		tail_header.prev_hash
	};
	store.get_block(&genesis_hash).map_err(|e| {
		format!(
			"chain data is incomplete: genesis full block {} is missing: {}",
			genesis_hash, e
		)
	})?;
	println!(
		"Archive preflight passed: full-block history includes genesis and ends at height {}.",
		head.height
	);
	Ok(head)
}

fn keychain_from_mnemonic(words: &str) -> Result<ExtKeychain, String> {
	let entropy =
		mnemonic::to_entropy(words.trim()).map_err(|e| format!("invalid wallet seed: {}", e))?;
	ExtKeychain::from_seed(&entropy, false)
		.map_err(|e| format!("unable to derive wallet keychain: {}", e))
}

fn read_visible_line(prompt: &str) -> Result<String, String> {
	print!("{}", prompt);
	io::stdout()
		.flush()
		.map_err(|e| format!("unable to display prompt: {}", e))?;
	let mut value = String::new();
	io::stdin()
		.read_line(&mut value)
		.map_err(|e| format!("unable to read input: {}", e))?;
	Ok(value.trim_end().to_owned())
}

fn find_owned_outputs<K: Keychain>(
	store: &ChainStore,
	head_hash: Hash,
	head_height: u64,
	keychain: &K,
) -> Result<Vec<HistoricalOutput>, String> {
	let legacy_builder = LegacyProofBuilder::new(keychain);
	let builder = ProofBuilder::new(keychain);
	let mut outputs = Vec::new();
	walk_chain(store, head_hash, |header, block| {
		show_progress("outputs", head_height, header.height);
		for output in block.outputs() {
			let rewind = rewind_output(keychain, &legacy_builder, &builder, header.height, output)?;
			if let Some((amount, key_id, switch)) = rewind {
				clear_progress_line();
				println!(
					"FOUND output: height={} amount={:.9} GRIN commitment={}",
					header.height,
					amount as f64 / 1_000_000_000.0,
					output.commitment().to_hex()
				);
				outputs.push(historical_output(header, output, amount, key_id, switch));
			}
		}
		Ok(())
	})?;
	println!();
	Ok(outputs)
}

fn rewind_output<K: Keychain>(
	keychain: &K,
	legacy_builder: &LegacyProofBuilder<'_, K>,
	builder: &ProofBuilder<'_, K>,
	height: u64,
	output: &grin_core::core::Output,
) -> Result<Option<(u64, Identifier, SwitchCommitmentType)>, String> {
	let rewind = if valid_header_version(height.saturating_sub(2 * WEEK_HEIGHT), HeaderVersion(1)) {
		proof::rewind(
			keychain.secp(),
			legacy_builder,
			output.commitment(),
			None,
			output.proof(),
		)
		.map_err(|e| e.to_string())?
	} else {
		None
	};
	match rewind {
		Some(rewind) => Ok(Some(rewind)),
		None => proof::rewind(
			keychain.secp(),
			builder,
			output.commitment(),
			None,
			output.proof(),
		)
		.map_err(|e| e.to_string()),
	}
}

fn historical_output(
	header: &BlockHeader,
	output: &grin_core::core::Output,
	amount: u64,
	key_id: Identifier,
	switch: SwitchCommitmentType,
) -> HistoricalOutput {
	HistoricalOutput {
		commitment: output.commitment().to_hex(),
		amount,
		key_id: key_id.to_string(),
		switch_type: format!("{:?}", switch),
		is_coinbase: output.is_coinbase(),
		created_height: header.height,
		created_at: header.timestamp.to_rfc3339(),
		spendable_height: if output.is_coinbase() {
			header.height + global::coinbase_maturity()
		} else {
			header.height
		},
		spent_height: None,
		spent_at: None,
	}
}

fn find_spends(
	store: &ChainStore,
	head_hash: Hash,
	head_height: u64,
	outputs: &mut [HistoricalOutput],
) -> Result<(), String> {
	let mut owned: HashMap<String, usize> = outputs
		.iter()
		.enumerate()
		.map(|(idx, output)| (output.commitment.clone(), idx))
		.collect();
	walk_chain(store, head_hash, |header, block| {
		show_progress("spends", head_height, header.height);
		let inputs: Vec<CommitWrapper> = block.inputs().into();
		for input in inputs {
			let commit = input.commitment().to_hex();
			if let Some(idx) = owned.remove(&commit) {
				outputs[idx].spent_height = Some(header.height);
				outputs[idx].spent_at = Some(header.timestamp.to_rfc3339());
				clear_progress_line();
				println!(
					"FOUND spend: height={} amount={:.9} GRIN commitment={}",
					header.height,
					outputs[idx].amount as f64 / 1_000_000_000.0,
					commit
				);
			}
		}
		Ok(())
	})?;
	println!();
	Ok(())
}

fn show_progress(pass: &str, head_height: u64, current_height: u64) {
	let processed = head_height.saturating_sub(current_height);
	if processed % 10_000 == 0 || current_height == 0 {
		let percent = if head_height == 0 {
			100.0
		} else {
			processed as f64 * 100.0 / head_height as f64
		};
		print!(
			"\r\x1b[2KProgress ({}): {:.2}% - {} / {} blocks",
			pass, percent, processed, head_height
		);
		let _ = io::stdout().flush();
	}
}

fn clear_progress_line() {
	print!("\r\x1b[2K");
	let _ = io::stdout().flush();
}

fn walk_chain<F>(store: &ChainStore, mut hash: Hash, mut visit: F) -> Result<(), String>
where
	F: FnMut(&BlockHeader, &grin_core::core::Block) -> Result<(), String>,
{
	loop {
		let header = store
			.get_block_header(&hash)
			.map_err(|e| format!("missing header {}: {}", hash, e))?;
		let block = store.get_block(&hash).map_err(|e| {
			format!(
				"missing full block {} at height {} (archive data required): {}",
				hash, header.height, e
			)
		})?;
		visit(&header, &block)?;
		if header.height == 0 {
			break;
		}
		hash = header.prev_hash;
	}
	Ok(())
}

fn build_balance_timeline(outputs: &[HistoricalOutput]) -> Result<Vec<BalancePoint>, String> {
	let mut changes: BTreeMap<u64, BalanceChange> = BTreeMap::new();
	for output in outputs {
		let created = changes.entry(output.created_height).or_default();
		created.timestamp = output.created_at.clone();
		created.received = created
			.received
			.checked_add(output.amount)
			.ok_or_else(|| "received balance overflow".to_owned())?;
		if let (Some(height), Some(timestamp)) = (output.spent_height, &output.spent_at) {
			let spent = changes.entry(height).or_default();
			spent.timestamp = timestamp.clone();
			spent.spent = spent
				.spent
				.checked_add(output.amount)
				.ok_or_else(|| "spent balance overflow".to_owned())?;
		}
	}

	let mut balance = 0_u64;
	let mut timeline = Vec::with_capacity(changes.len());
	for (height, change) in changes {
		balance = balance
			.checked_add(change.received)
			.and_then(|value| value.checked_sub(change.spent))
			.ok_or_else(|| format!("invalid balance transition at height {}", height))?;
		timeline.push(BalancePoint {
			height,
			timestamp: change.timestamp,
			received: change.received,
			spent: change.spent,
			balance,
		});
	}
	Ok(timeline)
}

#[cfg(test)]
mod tests {
	use super::*;
	use grin_core::core::{Output, OutputFeatures};
	use grin_keychain::ExtKeychain;

	fn output(amount: u64, created: u64, spent: Option<u64>) -> HistoricalOutput {
		HistoricalOutput {
			commitment: format!("commit-{}-{}", amount, created),
			amount,
			key_id: "m/0/0".to_owned(),
			switch_type: "Regular".to_owned(),
			is_coinbase: false,
			created_height: created,
			created_at: format!("created-{}", created),
			spendable_height: created,
			spent_height: spent,
			spent_at: spent.map(|height| format!("spent-{}", height)),
		}
	}

	#[test]
	fn balance_timeline_combines_changes_at_each_height() {
		let outputs = vec![
			output(10, 2, Some(5)),
			output(7, 2, None),
			output(4, 5, Some(8)),
		];
		let timeline = build_balance_timeline(&outputs).unwrap();

		assert_eq!(timeline.len(), 3);
		assert_eq!(timeline[0].height, 2);
		assert_eq!(timeline[0].received, 17);
		assert_eq!(timeline[0].balance, 17);
		assert_eq!(timeline[1].height, 5);
		assert_eq!(timeline[1].received, 4);
		assert_eq!(timeline[1].spent, 10);
		assert_eq!(timeline[1].balance, 11);
		assert_eq!(timeline[2].height, 8);
		assert_eq!(timeline[2].balance, 7);
	}

	#[test]
	fn balance_timeline_rejects_spending_before_receiving() {
		let outputs = vec![output(10, 5, Some(2))];
		assert!(build_balance_timeline(&outputs).is_err());
	}

	#[test]
	fn rewinds_output_created_by_wallet() {
		global::set_local_chain_type(global::ChainTypes::Mainnet);
		let keychain = ExtKeychain::from_seed(&[7; 32], false).unwrap();
		let key_id = ExtKeychain::derive_key_id(3, 1, 2, 3, 0);
		let amount = 42;
		let switch = SwitchCommitmentType::Regular;
		let commit = keychain.commit(amount, &key_id, switch).unwrap();
		let builder = ProofBuilder::new(&keychain);
		let proof =
			proof::create(&keychain, &builder, amount, &key_id, switch, commit, None).unwrap();
		let output = Output::new(OutputFeatures::Plain, commit, proof);
		let legacy_builder = LegacyProofBuilder::new(&keychain);

		let recovered = rewind_output(&keychain, &legacy_builder, &builder, u64::MAX, &output)
			.unwrap()
			.unwrap();
		assert_eq!(recovered.0, amount);
		assert_eq!(recovered.1, key_id);
		assert_eq!(recovered.2, switch);
	}

	#[test]
	fn mnemonic_uses_wallet_entropy_directly() {
		let words =
			"abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
		let entropy = mnemonic::to_entropy(words).unwrap();
		let expected = ExtKeychain::from_seed(&entropy, false).unwrap();
		let actual = keychain_from_mnemonic(words).unwrap();

		assert_eq!(actual.public_root_key(), expected.public_root_key());
	}
}

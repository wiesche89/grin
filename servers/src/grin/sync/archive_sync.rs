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

use crate::common::types::ArchiveSyncConfig;
use crate::core::core::hash::{Hash, Hashed};
use crate::core::core::Block;
use crate::core::ser::{self, ProtocolVersion};
use crate::p2p;
use crate::util::{Mutex, RwLock};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

const ATTEMPT_HISTORY_LIMIT: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptKind {
	Primary,
	Hedge,
	Retry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseMatch {
	Active,
	Duplicate,
	Unsolicited,
}

#[derive(Clone, Debug)]
struct Attempt {
	id: u64,
	peer: SocketAddr,
	requested_at: Instant,
	deadline: Instant,
	kind: AttemptKind,
}

#[derive(Clone, Debug)]
struct RequestState {
	height: u64,
	attempts: Vec<Attempt>,
}

#[derive(Clone, Debug)]
struct CompletedAttempt {
	hash: Hash,
	peer: SocketAddr,
	expires_at: Instant,
}

#[derive(Clone, Debug, Default)]
struct PeerPerformance {
	requested: u64,
	completed: u64,
	timed_out: u64,
	useful_bytes: u64,
	response_ns: u64,
}

#[derive(Default)]
struct SchedulerState {
	requests: HashMap<Hash, RequestState>,
	completed: VecDeque<CompletedAttempt>,
	peer_inflight: HashMap<SocketAddr, usize>,
	timed_out: VecDeque<SocketAddr>,
	timed_out_total: u64,
	reassigned: u64,
	next_attempt_id: u64,
	peer_performance: HashMap<SocketAddr, PeerPerformance>,
}

/// Bounded request accounting shared by body sync and network ingestion.
pub struct ArchiveRequestScheduler {
	config: ArchiveSyncConfig,
	state: Mutex<SchedulerState>,
}

/// Result of handing a network block to the archive pipeline.
pub enum BlockAcceptance {
	Queued,
	Consumed,
}

struct QueuedBlock {
	block: Block,
	peer: p2p::PeerAddr,
	bytes: usize,
	queued_at: Instant,
}

struct CheckedBlock {
	block: Result<crate::chain::ValidatedBlock, crate::chain::Error>,
	hash: Hash,
	height: u64,
	peer: p2p::PeerAddr,
	bytes: usize,
}

/// Bounded download and validation pipeline used only for archive body sync.
pub struct ArchiveSyncPipeline {
	config: ArchiveSyncConfig,
	scheduler: Arc<ArchiveRequestScheduler>,
	queue: mpsc::SyncSender<QueuedBlock>,
	queued_blocks: Arc<AtomicUsize>,
	queued_bytes: Arc<AtomicUsize>,
	peers: Arc<RwLock<Option<Weak<p2p::Peers>>>>,
	received: AtomicU64,
	validated: Arc<AtomicU64>,
	rejected: Arc<AtomicU64>,
	applied: Arc<AtomicU64>,
	queue_wait_ns: Arc<AtomicU64>,
	validation_ns: Arc<AtomicU64>,
	apply_ns: Arc<AtomicU64>,
	duplicates: Arc<AtomicU64>,
	queue_rejected: AtomicU64,
	stale_batches: Arc<AtomicU64>,
	fallback_batches: Arc<AtomicU64>,
	committed_batches: Arc<AtomicU64>,
	max_reorder_depth: Arc<AtomicUsize>,
	active: Arc<AtomicBool>,
	worker_failures: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ArchiveSyncStats {
	pub received: u64,
	pub validated: u64,
	pub rejected: u64,
	pub applied: u64,
	pub outstanding: usize,
	pub queued_blocks: usize,
	pub queued_bytes: usize,
	pub queue_wait_ms: u64,
	pub validation_ms: u64,
	pub apply_ms: u64,
	pub duplicates: u64,
	pub timed_out: u64,
	pub reassigned: u64,
	pub queue_rejected: u64,
	pub stale_batches: u64,
	pub fallback_batches: u64,
	pub committed_batches: u64,
	pub max_reorder_depth: usize,
	pub worker_failures: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct ArchivePeerStats {
	pub peer: SocketAddr,
	pub requested: u64,
	pub completed: u64,
	pub timed_out: u64,
	pub useful_bytes: u64,
	pub average_response_ms: u64,
	pub bytes_per_second: u64,
}

impl ArchiveSyncPipeline {
	pub fn new(mut config: ArchiveSyncConfig, chain: Arc<crate::chain::Chain>) -> Arc<Self> {
		config.request_window = config.request_window.max(1);
		config.peer_inflight_limit = config.peer_inflight_limit.max(1);
		config.request_timeout_ms = config.request_timeout_ms.max(1);
		config.queue_blocks = config.queue_blocks.max(1);
		config.queue_bytes = config.queue_bytes.max(1);
		config.batch_blocks = config.batch_blocks.max(1);
		config.batch_bytes = config.batch_bytes.max(1);
		let (queue, receiver) = mpsc::sync_channel::<QueuedBlock>(config.queue_blocks);
		let (checked_tx, checked_rx) = mpsc::sync_channel::<CheckedBlock>(config.queue_blocks);
		let queued_blocks = Arc::new(AtomicUsize::new(0));
		let queued_bytes = Arc::new(AtomicUsize::new(0));
		let peers = Arc::new(RwLock::new(None));
		let validated = Arc::new(AtomicU64::new(0));
		let rejected = Arc::new(AtomicU64::new(0));
		let applied = Arc::new(AtomicU64::new(0));
		let queue_wait_ns = Arc::new(AtomicU64::new(0));
		let validation_ns = Arc::new(AtomicU64::new(0));
		let apply_ns = Arc::new(AtomicU64::new(0));
		let duplicates = Arc::new(AtomicU64::new(0));
		let stale_batches = Arc::new(AtomicU64::new(0));
		let fallback_batches = Arc::new(AtomicU64::new(0));
		let committed_batches = Arc::new(AtomicU64::new(0));
		let max_reorder_depth = Arc::new(AtomicUsize::new(0));
		let active = Arc::new(AtomicBool::new(false));
		let worker_failures = Arc::new(AtomicU64::new(0));
		let scheduler = Arc::new(ArchiveRequestScheduler::new(config.clone()));

		let receiver = Arc::new(Mutex::new(receiver));
		let workers = if !config.enabled {
			0
		} else if config.validation_workers == 0 {
			thread::available_parallelism()
				.map(|count| count.get().saturating_sub(1).clamp(1, 4))
				.unwrap_or(1)
		} else {
			config.validation_workers.max(1)
		};
		for index in 0..workers {
			let receiver = receiver.clone();
			let checked_tx = checked_tx.clone();
			let chain = chain.clone();
			let validated = validated.clone();
			let rejected = rejected.clone();
			let scheduler = scheduler.clone();
			let queue_wait_ns = queue_wait_ns.clone();
			let validation_ns = validation_ns.clone();
			let queued_blocks = queued_blocks.clone();
			let queued_bytes = queued_bytes.clone();
			let duplicates = duplicates.clone();
			let worker_failures = worker_failures.clone();
			let _ = thread::Builder::new()
				.name(format!("archive_validate_{}", index))
				.spawn(move || {
					let secp = crate::util::secp::Secp256k1::with_caps(
						crate::util::secp::ContextFlag::Commit,
					);
					loop {
						let queued = match receiver.lock().recv() {
							Ok(queued) => queued,
							Err(_) => break,
						};
						queue_wait_ns.fetch_add(
							queued.queued_at.elapsed().as_nanos().min(u64::MAX as u128) as u64,
							Ordering::Relaxed,
						);
						let hash = queued.block.hash();
						let height = queued.block.header.height;
						let validation_started = Instant::now();
						let fallback = queued.block.clone();
						let block = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
							chain.validate_block_with_secp(queued.block, &secp)
						}))
						.unwrap_or_else(|_| {
							worker_failures.fetch_add(1, Ordering::Relaxed);
							warn!("archive validation worker failed, retrying synchronously");
							chain.validate_block(fallback)
						});
						validation_ns.fetch_add(
							validation_started
								.elapsed()
								.as_nanos()
								.min(u64::MAX as u128) as u64,
							Ordering::Relaxed,
						);
						if block.is_ok() {
							if !scheduler.complete(
								&hash,
								queued.peer.0,
								queued.bytes,
								Instant::now(),
							) {
								duplicates.fetch_add(1, Ordering::Relaxed);
								queued_blocks.fetch_sub(1, Ordering::AcqRel);
								queued_bytes.fetch_sub(queued.bytes, Ordering::AcqRel);
								continue;
							}
							validated.fetch_add(1, Ordering::Relaxed);
						} else {
							rejected.fetch_add(1, Ordering::Relaxed);
							scheduler.reject_attempt(&hash, queued.peer.0);
						}
						if checked_tx
							.send(CheckedBlock {
								block,
								hash,
								height,
								peer: queued.peer,
								bytes: queued.bytes,
							})
							.is_err()
						{
							break;
						}
					}
				});
		}
		drop(checked_tx);

		Self::spawn_apply_worker(
			chain,
			checked_rx,
			queued_blocks.clone(),
			queued_bytes.clone(),
			peers.clone(),
			applied.clone(),
			config.batch_blocks,
			config.batch_bytes,
			apply_ns.clone(),
			stale_batches.clone(),
			fallback_batches.clone(),
			committed_batches.clone(),
			max_reorder_depth.clone(),
			active.clone(),
		);

		Arc::new(Self {
			scheduler,
			config,
			queue,
			queued_blocks,
			queued_bytes,
			peers,
			received: AtomicU64::new(0),
			validated,
			rejected,
			applied,
			queue_wait_ns,
			validation_ns,
			apply_ns,
			duplicates,
			queue_rejected: AtomicU64::new(0),
			stale_batches,
			fallback_batches,
			committed_batches,
			max_reorder_depth,
			active,
			worker_failures,
		})
	}

	pub fn init(&self, peers: Arc<p2p::Peers>) {
		*self.peers.write() = Some(Arc::downgrade(&peers));
	}

	pub fn request(&self, hash: Hash, height: u64, peer: SocketAddr) -> Option<AttemptKind> {
		if !self.config.enabled {
			return Some(AttemptKind::Primary);
		}
		if self.queued_blocks.load(Ordering::Acquire) >= self.config.queue_blocks
			|| self.queued_bytes.load(Ordering::Acquire) >= self.config.queue_bytes
		{
			return None;
		}
		self.scheduler.request(hash, height, peer, Instant::now())
	}

	pub fn cancel(&self, hash: &Hash, peer: SocketAddr) {
		if self.config.enabled {
			self.scheduler.cancel(hash, peer);
		}
	}

	pub fn outstanding(&self) -> usize {
		self.scheduler.outstanding()
	}

	pub fn take_timed_out_peers(&self) -> Vec<SocketAddr> {
		let mut state = self.scheduler.state.lock();
		state.timed_out.drain(..).collect()
	}

	pub fn enabled(&self) -> bool {
		self.config.enabled && self.active.load(Ordering::Acquire)
	}

	pub fn configured(&self) -> bool {
		self.config.enabled
	}

	pub fn set_active(&self, active: bool) {
		self.active.store(active, Ordering::Release);
		if !active {
			let mut state = self.scheduler.state.lock();
			state.requests.clear();
			state.peer_inflight.clear();
		}
	}

	pub fn request_window(&self) -> usize {
		self.config.request_window
	}

	pub fn accepts(&self, block: Block, peer: p2p::PeerAddr) -> Result<BlockAcceptance, Block> {
		if !self.config.enabled {
			return Err(block);
		}
		let hash = block.hash();
		match self.scheduler.match_response(&hash, peer.0) {
			ResponseMatch::Duplicate => {
				self.duplicates.fetch_add(1, Ordering::Relaxed);
				return Ok(BlockAcceptance::Consumed);
			}
			ResponseMatch::Unsolicited => return Err(block),
			ResponseMatch::Active => {}
		}

		let bytes = ser::ser_vec(&block, ProtocolVersion::local())
			.map(|block| block.len())
			.unwrap_or(self.config.queue_bytes);
		let block_reserved = self.reserve_block();
		if !block_reserved || !self.reserve_bytes(bytes) {
			if block_reserved {
				self.queued_blocks.fetch_sub(1, Ordering::AcqRel);
			}
			self.queue_rejected.fetch_add(1, Ordering::Relaxed);
			self.scheduler.reject_attempt(&hash, peer.0);
			return Ok(BlockAcceptance::Consumed);
		}
		match self.queue.try_send(QueuedBlock {
			block,
			peer,
			bytes,
			queued_at: Instant::now(),
		}) {
			Ok(()) => {
				self.received.fetch_add(1, Ordering::Relaxed);
				Ok(BlockAcceptance::Queued)
			}
			Err(_) => {
				self.queued_blocks.fetch_sub(1, Ordering::AcqRel);
				self.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
				self.queue_rejected.fetch_add(1, Ordering::Relaxed);
				self.scheduler.reject_attempt(&hash, peer.0);
				Ok(BlockAcceptance::Consumed)
			}
		}
	}

	pub fn stats(&self) -> ArchiveSyncStats {
		let scheduler = self.scheduler.state.lock();
		ArchiveSyncStats {
			received: self.received.load(Ordering::Relaxed),
			validated: self.validated.load(Ordering::Relaxed),
			rejected: self.rejected.load(Ordering::Relaxed),
			applied: self.applied.load(Ordering::Relaxed),
			outstanding: self.outstanding(),
			queued_blocks: self.queued_blocks.load(Ordering::Relaxed),
			queued_bytes: self.queued_bytes.load(Ordering::Relaxed),
			queue_wait_ms: self.queue_wait_ns.load(Ordering::Relaxed) / 1_000_000,
			validation_ms: self.validation_ns.load(Ordering::Relaxed) / 1_000_000,
			apply_ms: self.apply_ns.load(Ordering::Relaxed) / 1_000_000,
			duplicates: self.duplicates.load(Ordering::Relaxed),
			timed_out: scheduler.timed_out_total,
			reassigned: scheduler.reassigned,
			queue_rejected: self.queue_rejected.load(Ordering::Relaxed),
			stale_batches: self.stale_batches.load(Ordering::Relaxed),
			fallback_batches: self.fallback_batches.load(Ordering::Relaxed),
			committed_batches: self.committed_batches.load(Ordering::Relaxed),
			max_reorder_depth: self.max_reorder_depth.load(Ordering::Relaxed),
			worker_failures: self.worker_failures.load(Ordering::Relaxed),
		}
	}

	pub fn peer_stats(&self) -> Vec<ArchivePeerStats> {
		self.scheduler.peer_stats()
	}

	pub fn peer_score(&self, peer: SocketAddr) -> u128 {
		self.scheduler.peer_score(peer)
	}

	fn reserve_bytes(&self, bytes: usize) -> bool {
		let mut current = self.queued_bytes.load(Ordering::Acquire);
		loop {
			let Some(next) = current.checked_add(bytes) else {
				return false;
			};
			if next > self.config.queue_bytes {
				return false;
			}
			match self.queued_bytes.compare_exchange_weak(
				current,
				next,
				Ordering::AcqRel,
				Ordering::Acquire,
			) {
				Ok(_) => return true,
				Err(actual) => current = actual,
			}
		}
	}

	fn reserve_block(&self) -> bool {
		self.queued_blocks
			.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
				(current < self.config.queue_blocks).then_some(current + 1)
			})
			.is_ok()
	}

	fn spawn_apply_worker(
		chain: Arc<crate::chain::Chain>,
		receiver: mpsc::Receiver<CheckedBlock>,
		queued_blocks: Arc<AtomicUsize>,
		queued_bytes: Arc<AtomicUsize>,
		peers: Arc<RwLock<Option<Weak<p2p::Peers>>>>,
		applied: Arc<AtomicU64>,
		batch_blocks: usize,
		batch_bytes: usize,
		apply_ns: Arc<AtomicU64>,
		stale_batches: Arc<AtomicU64>,
		fallback_batches: Arc<AtomicU64>,
		committed_batches: Arc<AtomicU64>,
		max_reorder_depth: Arc<AtomicUsize>,
		active: Arc<AtomicBool>,
	) {
		let _ = thread::Builder::new()
			.name("archive_apply".to_string())
			.spawn(move || {
				let mut ready = BTreeMap::new();
				while let Ok(checked) = receiver.recv() {
					if !active.load(Ordering::Acquire) {
						let buffered_blocks = ready.len();
						let buffered_bytes =
							ready.values().map(|(_, _, bytes)| *bytes).sum::<usize>();
						ready.clear();
						queued_blocks.fetch_sub(buffered_blocks + 1, Ordering::AcqRel);
						queued_bytes.fetch_sub(
							buffered_bytes.saturating_add(checked.bytes),
							Ordering::AcqRel,
						);
						continue;
					}
					match checked.block {
						Ok(block) => {
							if let Some((_, _, previous_bytes)) =
								ready.insert(checked.height, (block, checked.peer, checked.bytes))
							{
								queued_blocks.fetch_sub(1, Ordering::AcqRel);
								queued_bytes.fetch_sub(previous_bytes, Ordering::AcqRel);
							}
							max_reorder_depth.fetch_max(ready.len(), Ordering::Relaxed);
						}
						Err(error) => {
							queued_blocks.fetch_sub(1, Ordering::AcqRel);
							queued_bytes.fetch_sub(checked.bytes, Ordering::AcqRel);
							warn!(
								"archive sync rejected block {} from {}: {}",
								checked.hash, checked.peer, error
							);
							if error.is_bad_data() {
								Self::ban(&peers, checked.peer);
							}
							continue;
						}
					}

					loop {
						let next_height = match chain.head() {
							Ok(head) => head.height + 1,
							Err(error) => {
								error!("archive sync cannot read chain head: {}", error);
								break;
							}
						};
						if !ready.contains_key(&next_height) {
							break;
						}
						let mut blocks = vec![];
						let mut sources = vec![];
						let mut bytes = 0usize;
						for height in next_height.. {
							let Some((_, _, block_bytes)) = ready.get(&height) else {
								break;
							};
							if !blocks.is_empty()
								&& (blocks.len() >= batch_blocks
									|| bytes.saturating_add(*block_bytes) > batch_bytes)
							{
								break;
							}
							let (block, peer, block_bytes) =
								ready.remove(&height).expect("entry checked above");
							blocks.push(block);
							sources.push(peer);
							bytes += block_bytes;
						}
						let count = blocks.len() as u64;
						let fallback = blocks.clone();
						if !active.load(Ordering::Acquire) {
							let buffered_blocks = ready.len();
							let buffered_bytes =
								ready.values().map(|(_, _, bytes)| *bytes).sum::<usize>();
							ready.clear();
							queued_blocks
								.fetch_sub(count as usize + buffered_blocks, Ordering::AcqRel);
							queued_bytes
								.fetch_sub(bytes.saturating_add(buffered_bytes), Ordering::AcqRel);
							break;
						}
						let apply_started = Instant::now();
						match chain.process_validated_batch(blocks, crate::chain::Options::SYNC) {
							Ok(_) => {
								queued_blocks.fetch_sub(count as usize, Ordering::AcqRel);
								queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
								applied.fetch_add(count, Ordering::Relaxed);
								committed_batches.fetch_add(1, Ordering::Relaxed);
							}
							Err(crate::chain::Error::StaleValidatedBlock) => {
								let stale_blocks = ready.len();
								queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
								let stale_bytes =
									ready.values().map(|(_, _, bytes)| *bytes).sum::<usize>();
								ready.clear();
								queued_blocks
									.fetch_sub(count as usize + stale_blocks, Ordering::AcqRel);
								queued_bytes.fetch_sub(stale_bytes, Ordering::AcqRel);
								stale_batches.fetch_add(1, Ordering::Relaxed);
								debug!(
									"discarding archive block batch at {} after chain context changed",
									next_height
								);
								break;
							}
							Err(error) => {
								queued_blocks.fetch_sub(count as usize, Ordering::AcqRel);
								queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
								fallback_batches.fetch_add(1, Ordering::Relaxed);
								warn!(
									"archive sync could not apply batch at {}: {}",
									next_height, error
								);
								for (block, peer) in fallback.into_iter().zip(sources) {
									match chain.process_block(
										block.into_block(),
										crate::chain::Options::SYNC,
									) {
										Ok(_) => {
											applied.fetch_add(1, Ordering::Relaxed);
										}
										Err(error) if error.is_bad_data() => {
											Self::ban(&peers, peer);
											break;
										}
										Err(_) => break,
									}
								}
								break;
							}
						}
						apply_ns.fetch_add(
							apply_started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
							Ordering::Relaxed,
						);
					}
				}
			});
	}

	fn ban(peers: &RwLock<Option<Weak<p2p::Peers>>>, peer: p2p::PeerAddr) {
		if let Some(peers) = peers.read().as_ref().and_then(Weak::upgrade) {
			let _ = peers.ban_peer(peer, p2p::types::ReasonForBan::BadBlock);
		}
	}
}

impl ArchiveRequestScheduler {
	pub fn new(config: ArchiveSyncConfig) -> Self {
		Self {
			config,
			state: Mutex::new(SchedulerState::default()),
		}
	}

	pub fn outstanding(&self) -> usize {
		self.state.lock().requests.len()
	}

	/// Record a primary, retry, or hedge attempt if all resource limits permit it.
	pub fn request(
		&self,
		hash: Hash,
		height: u64,
		peer: SocketAddr,
		now: Instant,
	) -> Option<AttemptKind> {
		let mut state = self.state.lock();
		Self::expire_attempts(
			&mut state,
			now,
			Duration::from_millis(self.config.request_timeout_ms),
		);

		if state.peer_inflight.get(&peer).copied().unwrap_or(0) >= self.config.peer_inflight_limit {
			return None;
		}

		if !state.requests.contains_key(&hash) && state.requests.len() >= self.config.request_window
		{
			return None;
		}

		let hedge_after = Duration::from_millis(self.config.hedge_timeout_ms);
		let kind = match state.requests.get(&hash) {
			None if state
				.completed
				.iter()
				.any(|attempt| attempt.hash == hash && attempt.peer == peer) =>
			{
				return None;
			}
			None if state.completed.iter().any(|attempt| attempt.hash == hash) => {
				AttemptKind::Retry
			}
			None => AttemptKind::Primary,
			Some(request) if request.height != height => return None,
			Some(request) if request.attempts.iter().any(|x| x.peer == peer) => return None,
			Some(request) if request.attempts.len() >= 2 => return None,
			Some(request)
				if request
					.attempts
					.iter()
					.any(|x| now.saturating_duration_since(x.requested_at) >= hedge_after) =>
			{
				AttemptKind::Hedge
			}
			Some(request) if request.attempts.is_empty() => AttemptKind::Retry,
			Some(_) => return None,
		};

		let deadline = now + Duration::from_millis(self.config.request_timeout_ms);
		if kind != AttemptKind::Primary {
			state.reassigned += 1;
		}
		let attempt_id = state.next_attempt_id;
		state.next_attempt_id = state.next_attempt_id.wrapping_add(1);
		state.peer_performance.entry(peer).or_default().requested += 1;
		state
			.requests
			.entry(hash)
			.or_insert_with(|| RequestState {
				height,
				attempts: vec![],
			})
			.attempts
			.push(Attempt {
				id: attempt_id,
				peer,
				requested_at: now,
				deadline,
				kind,
			});
		*state.peer_inflight.entry(peer).or_insert(0) += 1;
		Some(kind)
	}

	pub fn cancel(&self, hash: &Hash, peer: SocketAddr) {
		let mut state = self.state.lock();
		Self::remove_attempt(&mut state, hash, peer);
	}

	pub fn match_response(&self, hash: &Hash, peer: SocketAddr) -> ResponseMatch {
		let mut state = self.state.lock();
		let now = Instant::now();
		state.completed.retain(|attempt| now < attempt.expires_at);
		if state
			.requests
			.get(hash)
			.map(|x| x.attempts.iter().any(|attempt| attempt.peer == peer))
			.unwrap_or(false)
		{
			ResponseMatch::Active
		} else if state
			.completed
			.iter()
			.any(|attempt| &attempt.hash == hash && attempt.peer == peer)
		{
			ResponseMatch::Duplicate
		} else {
			ResponseMatch::Unsolicited
		}
	}

	/// Complete the hash after intrinsic validation. All other attempts become
	/// recognizable duplicates and stop consuming in-flight capacity.
	pub fn complete(
		&self,
		hash: &Hash,
		winning_peer: SocketAddr,
		bytes: usize,
		now: Instant,
	) -> bool {
		let mut state = self.state.lock();
		let Some(request) = state.requests.remove(hash) else {
			return false;
		};
		if let Some(attempt) = request
			.attempts
			.iter()
			.find(|attempt| attempt.peer == winning_peer)
		{
			trace!(
				"archive request {} ({:?} #{}) completed by {}",
				hash,
				attempt.kind,
				attempt.id,
				winning_peer
			);
			let peer = state.peer_performance.entry(winning_peer).or_default();
			peer.completed += 1;
			peer.useful_bytes = peer.useful_bytes.saturating_add(bytes as u64);
			peer.response_ns = peer.response_ns.saturating_add(
				now.saturating_duration_since(attempt.requested_at)
					.as_nanos()
					.min(u64::MAX as u128) as u64,
			);
		}
		for attempt in request.attempts {
			Self::decrement_peer(&mut state, attempt.peer);
			state.completed.push_back(CompletedAttempt {
				hash: *hash,
				peer: attempt.peer,
				expires_at: attempt.deadline,
			});
		}
		while state.completed.len() > ATTEMPT_HISTORY_LIMIT {
			state.completed.pop_front();
		}
		true
	}

	pub fn reject_attempt(&self, hash: &Hash, peer: SocketAddr) {
		let mut state = self.state.lock();
		Self::remove_attempt(&mut state, hash, peer);
	}

	fn expire_attempts(state: &mut SchedulerState, now: Instant, retry_delay: Duration) {
		state.completed.retain(|attempt| now < attempt.expires_at);
		let mut expired = vec![];
		for (hash, request) in &state.requests {
			for attempt in &request.attempts {
				if now >= attempt.deadline {
					expired.push((*hash, attempt.peer));
				}
			}
		}
		for (hash, peer) in expired {
			Self::remove_attempt(state, &hash, peer);
			state.completed.push_back(CompletedAttempt {
				hash,
				peer,
				expires_at: now + retry_delay,
			});
			state.timed_out.push_back(peer);
			state.timed_out_total += 1;
			state.peer_performance.entry(peer).or_default().timed_out += 1;
		}
		while state.completed.len() > ATTEMPT_HISTORY_LIMIT {
			state.completed.pop_front();
		}
	}

	fn remove_attempt(state: &mut SchedulerState, hash: &Hash, peer: SocketAddr) {
		let mut removed = false;
		let mut remove_request = false;
		if let Some(request) = state.requests.get_mut(hash) {
			let before = request.attempts.len();
			request.attempts.retain(|attempt| attempt.peer != peer);
			removed = before != request.attempts.len();
			remove_request = request.attempts.is_empty();
		}
		if removed {
			Self::decrement_peer(state, peer);
		}
		if remove_request {
			state.requests.remove(hash);
		}
	}

	fn decrement_peer(state: &mut SchedulerState, peer: SocketAddr) {
		if let Some(count) = state.peer_inflight.get_mut(&peer) {
			*count = count.saturating_sub(1);
			if *count == 0 {
				state.peer_inflight.remove(&peer);
			}
		}
	}

	fn peer_score(&self, peer: SocketAddr) -> u128 {
		let state = self.state.lock();
		let Some(stats) = state.peer_performance.get(&peer) else {
			return 0;
		};
		if stats.completed == 0 {
			return stats.timed_out as u128 * self.config.request_timeout_ms as u128 * 1_000_000;
		}
		let average_response = stats.response_ns as u128 / stats.completed as u128;
		let transfer_cost = if stats.useful_bytes == 0 {
			0
		} else {
			stats.response_ns as u128 * 1024 * 1024 / stats.useful_bytes as u128
		};
		let timeout_cost =
			stats.timed_out as u128 * self.config.request_timeout_ms as u128 * 1_000_000
				/ stats.requested.max(1) as u128;
		average_response + transfer_cost + timeout_cost
	}

	fn peer_stats(&self) -> Vec<ArchivePeerStats> {
		let state = self.state.lock();
		state
			.peer_performance
			.iter()
			.map(|(peer, stats)| ArchivePeerStats {
				peer: *peer,
				requested: stats.requested,
				completed: stats.completed,
				timed_out: stats.timed_out,
				useful_bytes: stats.useful_bytes,
				average_response_ms: if stats.completed == 0 {
					0
				} else {
					stats.response_ns / stats.completed / 1_000_000
				},
				bytes_per_second: if stats.response_ns == 0 {
					0
				} else {
					(stats.useful_bytes as u128 * 1_000_000_000 / stats.response_ns as u128)
						.min(u64::MAX as u128) as u64
				},
			})
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::core::core::hash::ZERO_HASH;

	fn config() -> ArchiveSyncConfig {
		ArchiveSyncConfig {
			enabled: true,
			request_window: 2,
			peer_inflight_limit: 1,
			hedge_timeout_ms: 100,
			request_timeout_ms: 500,
			..ArchiveSyncConfig::default()
		}
	}

	fn peer(port: u16) -> SocketAddr {
		format!("127.0.0.1:{}", port).parse().unwrap()
	}

	#[test]
	fn hedge_and_late_response_are_accounted_for() {
		let scheduler = ArchiveRequestScheduler::new(config());
		let now = Instant::now();
		assert_eq!(
			scheduler.request(ZERO_HASH, 1, peer(1), now),
			Some(AttemptKind::Primary)
		);
		assert_eq!(
			scheduler.request(ZERO_HASH, 1, peer(2), now + Duration::from_millis(99)),
			None
		);
		assert_eq!(
			scheduler.request(ZERO_HASH, 1, peer(2), now + Duration::from_millis(100)),
			Some(AttemptKind::Hedge)
		);
		assert_eq!(
			scheduler.match_response(&ZERO_HASH, peer(2)),
			ResponseMatch::Active
		);
		assert!(scheduler.complete(&ZERO_HASH, peer(2), 1024, now + Duration::from_millis(120)));
		assert!(!scheduler.complete(&ZERO_HASH, peer(1), 1024, now + Duration::from_millis(130)));
		assert_eq!(
			scheduler.match_response(&ZERO_HASH, peer(1)),
			ResponseMatch::Duplicate
		);
	}

	#[test]
	fn limits_and_timeout_release_capacity() {
		let scheduler = ArchiveRequestScheduler::new(config());
		let now = Instant::now();
		let other = Hash::from_hex(&"01".repeat(32)).unwrap();
		assert!(scheduler.request(ZERO_HASH, 1, peer(1), now).is_some());
		assert_eq!(scheduler.request(other, 2, peer(1), now), None);
		assert_eq!(
			scheduler.request(other, 2, peer(1), now + Duration::from_millis(500)),
			Some(AttemptKind::Primary)
		);
	}

	#[test]
	fn timed_out_hash_moves_to_another_peer() {
		let scheduler = ArchiveRequestScheduler::new(config());
		let now = Instant::now();
		assert!(scheduler.request(ZERO_HASH, 1, peer(1), now).is_some());
		assert_eq!(
			scheduler.request(ZERO_HASH, 1, peer(1), now + Duration::from_millis(500)),
			None
		);
		assert_eq!(
			scheduler.request(ZERO_HASH, 1, peer(2), now + Duration::from_millis(500)),
			Some(AttemptKind::Retry)
		);
	}
}

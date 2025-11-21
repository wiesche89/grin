// Copyright 2021 The Grin Developers
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

use chrono::prelude::{DateTime, Utc};
use chrono::Duration;
use std::cmp::Ordering;
use std::sync::Arc;

use crate::chain::{self, SyncState, SyncStatus};
use crate::common::types::Error;
use crate::core::core::hash::Hash;
use crate::core::pow::Difficulty;
use crate::p2p::{self, types::ReasonForBan, Capabilities, Peer};

pub struct HeaderSync {
	sync_state: Arc<SyncState>,
	peers: Arc<p2p::Peers>,
	chain: Arc<chain::Chain>,
	prev_header_sync: (DateTime<Utc>, u64, u64),
	syncing_peer: Option<Arc<Peer>>,
	stalling_ts: Option<DateTime<Utc>>,
}

const MAX_PARALLEL_HEADER_REQUESTS: usize = 1;

impl HeaderSync {
	pub fn new(
		sync_state: Arc<SyncState>,
		peers: Arc<p2p::Peers>,
		chain: Arc<chain::Chain>,
	) -> HeaderSync {
		HeaderSync {
			sync_state,
			peers,
			chain,
			prev_header_sync: (Utc::now(), 0, 0),
			syncing_peer: None,
			stalling_ts: None,
		}
	}

	pub fn check_run(&mut self, sync_head: chain::Tip) -> Result<bool, chain::Error> {
		// We only want to run header_sync for some sync states.
		let do_run = match self.sync_state.status() {
			SyncStatus::BodySync { .. }
			| SyncStatus::HeaderSync { .. }
			| SyncStatus::TxHashsetDone
			| SyncStatus::NoSync
			| SyncStatus::Initial
			| SyncStatus::AwaitingPeers(_) => true,
			_ => false,
		};

		if !do_run {
			return Ok(false);
		}

		let sync_peers = self.choose_sync_peers();

		if let Some(primary_peer) = sync_peers.first() {
			let (peer_height, peer_diff) =
				sync_peers
					.iter()
					.fold((0, Difficulty::zero()), |(height, diff), peer| {
						let peer_diff = peer.info.total_difficulty();
						if peer_diff > diff {
							(peer.info.height(), peer_diff)
						} else {
							(height, diff)
						}
					});

			// Quick check - nothing to sync if we are caught up with the peer.
			if peer_diff <= sync_head.total_difficulty {
				return Ok(false);
			}

			if !self.header_sync_due(sync_head) {
				return Ok(false);
			}

			self.sync_state.update(SyncStatus::HeaderSync {
				sync_head,
				highest_height: peer_height,
				highest_diff: peer_diff,
			});

			self.header_sync(sync_head, &sync_peers);
			self.syncing_peer = Some(primary_peer.clone());
		}
		Ok(true)
	}

	fn header_sync_due(&mut self, header_head: chain::Tip) -> bool {
		let now = Utc::now();
		let (timeout, latest_height, prev_height) = self.prev_header_sync;
		let pending = self.sync_state.pending_headers();

		if pending > 0 {
			trace!(
				"header_sync: currently processing {} pending headers, postponing new request",
				pending
			);
			return false;
		}
		let virtual_height = header_head.height + pending;

		let max_batch = (p2p::MAX_BLOCK_HEADERS as u64).saturating_sub(4);
		let newly_received = virtual_height.saturating_sub(prev_height);
		// received a full batch (or nearly full), can ask for more
		let all_headers_received = newly_received >= max_batch;
		// received only a partial batch, immediately ask another peer
		let partial_batch = newly_received > 0 && newly_received < max_batch;
		// no headers processed and we're past timeout, need to ask for more
		let stalling = header_head.height <= latest_height && now > timeout;

		// always enable header sync on initial state transition from NoSync / Initial
		let force_sync = match self.sync_state.status() {
			SyncStatus::NoSync | SyncStatus::Initial | SyncStatus::AwaitingPeers(_) => true,
			_ => false,
		};

		if force_sync || all_headers_received || stalling || partial_batch {
			self.prev_header_sync = (now + Duration::seconds(10), virtual_height, virtual_height);

			// save the stalling start time
			if stalling {
				if self.stalling_ts.is_none() {
					self.stalling_ts = Some(now);
				}
			} else {
				self.stalling_ts = None;
			}

			if all_headers_received {
				// reset the stalling start time if syncing goes well
				self.stalling_ts = None;
			} else if let Some(ref stalling_ts) = self.stalling_ts {
				if let Some(ref peer) = self.syncing_peer {
					match self.sync_state.status() {
						SyncStatus::HeaderSync { .. } | SyncStatus::BodySync { .. } => {
							// Ban this fraud peer which claims a higher work but can't send us the real headers
							if now > *stalling_ts + Duration::seconds(120)
								&& header_head.total_difficulty < peer.info.total_difficulty()
							{
								if let Err(e) = self
									.peers
									.ban_peer(peer.info.addr, ReasonForBan::FraudHeight)
								{
									error!("failed to ban peer {}: {:?}", peer.info.addr, e);
								}
								info!(
										"sync: ban a fraud peer: {}, claimed height: {}, total difficulty: {}",
										peer.info.addr,
										peer.info.height(),
										peer.info.total_difficulty(),
									);
							}
						}
						_ => (),
					}
				}
			}
			self.syncing_peer = None;
			true
		} else {
			// resetting the timeout as long as we progress
			if virtual_height > latest_height {
				self.prev_header_sync = (now + Duration::seconds(1), virtual_height, prev_height);
			}
			false
		}
	}

	fn choose_sync_peers(&self) -> Vec<Arc<Peer>> {
		let peers_iter = || {
			self.peers
				.iter()
				.with_capabilities(Capabilities::HEADER_HIST)
				.connected()
		};

		// Filter peers further based on max difficulty.
		let max_diff = peers_iter().max_difficulty().unwrap_or(Difficulty::zero());
		let candidates: Vec<_> = peers_iter()
			.with_difficulty(|x| x >= max_diff)
			.into_iter()
			.collect();

		if candidates.is_empty() {
			return vec![];
		}

		let mut outbound: Vec<_> = candidates
			.iter()
			.filter(|p| p.info.is_outbound())
			.cloned()
			.collect();
		let mut inbound: Vec<_> = candidates
			.into_iter()
			.filter(|p| p.info.is_inbound())
			.collect();

		let sort_by_speed = |peers: &mut Vec<Arc<Peer>>| {
			peers.sort_by(|a, b| {
				let score_a = a.info.header_sync_score();
				let score_b = b.info.header_sync_score();
				score_b.partial_cmp(&score_a).unwrap_or(Ordering::Equal)
			});
		};

		sort_by_speed(&mut outbound);
		sort_by_speed(&mut inbound);

		let mut result = Vec::new();
		for peer in outbound.into_iter().chain(inbound.into_iter()) {
			if peer.info.has_pending_header_request() {
				continue;
			}
			result.push(peer);
			if result.len() >= MAX_PARALLEL_HEADER_REQUESTS {
				break;
			}
		}

		if result.is_empty() {
			warn!("no suitable peer available for header sync");
		}

		result
	}

	fn header_sync(&self, sync_head: chain::Tip, peers: &[Arc<Peer>]) {
		for peer in peers {
			if peer.info.total_difficulty() > sync_head.total_difficulty {
				self.request_headers(sync_head, peer.clone());
			}
		}
	}

	/// Request some block headers from a peer to advance us.
	fn request_headers(&self, sync_head: chain::Tip, peer: Arc<Peer>) {
		if let Ok(locator) = self.get_locator(sync_head) {
			debug!(
				"sync: request_headers: asking {} for headers, {:?}",
				peer.info.addr, locator,
			);

			if peer.send_header_request(locator).is_ok() {
				peer.info.mark_header_request();
			}
		}
	}

	/// Build a locator based on header_head.
	fn get_locator(&self, sync_head: chain::Tip) -> Result<Vec<Hash>, Error> {
		let heights = get_locator_heights(sync_head.height);
		let locator = self.chain.get_locator_hashes(sync_head, &heights)?;
		Ok(locator)
	}
}

// current height back to 0 decreasing in powers of 2
fn get_locator_heights(height: u64) -> Vec<u64> {
	let mut current = height;
	let mut heights = vec![];
	while current > 0 {
		heights.push(current);
		if heights.len() >= (p2p::MAX_LOCATORS as usize) - 1 {
			break;
		}
		let next = 2u64.pow(heights.len() as u32);
		current = if current > next { current - next } else { 0 }
	}
	heights.push(0);
	heights
}

#[cfg(test)]
mod test {
	use super::*;

	#[test]
	fn test_get_locator_heights() {
		assert_eq!(get_locator_heights(0), vec![0]);
		assert_eq!(get_locator_heights(1), vec![1, 0]);
		assert_eq!(get_locator_heights(2), vec![2, 0]);
		assert_eq!(get_locator_heights(3), vec![3, 1, 0]);
		assert_eq!(get_locator_heights(10), vec![10, 8, 4, 0]);
		assert_eq!(get_locator_heights(100), vec![100, 98, 94, 86, 70, 38, 0]);
		assert_eq!(
			get_locator_heights(1000),
			vec![1000, 998, 994, 986, 970, 938, 874, 746, 490, 0]
		);
		// check the locator is still a manageable length, even for large numbers of
		// headers
		assert_eq!(
			get_locator_heights(10000),
			vec![10000, 9998, 9994, 9986, 9970, 9938, 9874, 9746, 9490, 8978, 7954, 5906, 1810, 0,]
		);
	}
}

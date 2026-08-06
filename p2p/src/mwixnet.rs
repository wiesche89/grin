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

//! MWixnet relay cache.

use crate::types::PeerAddr;
use crate::util::RwLock;
use chrono::Utc;
use core::ser::{self, ProtocolVersion};
use mwixnet_protocol::{Hash, OfferAnnouncement, PublicKey, RouteRelayItem, RouteRevocation};
use std::collections::{BTreeMap, HashMap, HashSet};

const ROUTE_CACHE_LIMIT: usize = 1_024;
const NEW_ROUTES_PER_MINUTE: usize = 128;
const NEW_MESSAGES_PER_MINUTE: usize = 16;
const OFFER_CACHE_LIMIT: usize = 1_024;
const NEW_OFFERS_PER_MINUTE: usize = 32;
const ROUTE_PREFIX: u8 = b'R';
const OFFER_PREFIX: u8 = b'O';
const ROUTE_CACHE_KEY: &[u8] = b"cache";
const OFFER_CACHE_KEY: &[u8] = b"offers";

/// Error returned by the relay cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteCacheError {
	Disabled,
	Invalid,
	RateLimited,
	Store,
}

#[derive(Clone)]
struct RouteGroup {
	announcement: mwixnet_protocol::RouteAnnouncement,
	status: Option<mwixnet_protocol::RouteStatus>,
	revocations: HashMap<PublicKey, RouteRevocation>,
}

struct PeerRate {
	minute: i64,
	new_routes: usize,
	new_offers: usize,
}

impl PeerRate {
	fn new(minute: i64) -> Self {
		Self {
			minute,
			new_routes: 0,
			new_offers: 0,
		}
	}

	fn reset(&mut self, minute: i64) {
		if self.minute != minute {
			*self = Self::new(minute);
		}
	}
}

struct ApiRate {
	minute: i64,
	messages: usize,
	new_routes: usize,
	new_offers: usize,
}

fn route_rank(route_id: Hash, group: &RouteGroup) -> (bool, u64, Hash) {
	(
		!group.revocations.is_empty(),
		group.announcement.last_verified,
		route_id,
	)
}

/// Validated routes and offers received from peers.
pub struct RouteCache {
	enabled: bool,
	allowlist: HashSet<PublicKey>,
	routes: RwLock<BTreeMap<Hash, RouteGroup>>,
	offers: RwLock<BTreeMap<Hash, OfferAnnouncement>>,
	peer_rates: RwLock<HashMap<PeerAddr, PeerRate>>,
	updates: RwLock<HashMap<Hash, i64>>,
	offer_updates: RwLock<HashMap<(u8, PublicKey), i64>>,
	revocation_updates: RwLock<HashMap<(Hash, PublicKey), i64>>,
	api_rates: RwLock<HashMap<PeerAddr, ApiRate>>,
	store: Option<grin_store::Store>,
}

impl RouteCache {
	pub fn new(enabled: bool, allowlist: Vec<PublicKey>) -> Self {
		Self {
			enabled,
			allowlist: allowlist.into_iter().collect(),
			routes: RwLock::new(BTreeMap::new()),
			offers: RwLock::new(BTreeMap::new()),
			peer_rates: RwLock::new(HashMap::new()),
			updates: RwLock::new(HashMap::new()),
			offer_updates: RwLock::new(HashMap::new()),
			revocation_updates: RwLock::new(HashMap::new()),
			api_rates: RwLock::new(HashMap::new()),
			store: None,
		}
	}

	pub fn open(
		enabled: bool,
		allowlist: Vec<PublicKey>,
		db_root: &str,
	) -> Result<Self, grin_store::Error> {
		let store = grin_store::Store::new(
			db_root,
			Some("mwixnet"),
			Some("routes"),
			vec![ROUTE_PREFIX, OFFER_PREFIX],
			None,
			None,
		)?;
		let saved_routes: Option<mwixnet_protocol::MwixnetRoutes> =
			store.get_ser(Some(ROUTE_PREFIX), ROUTE_CACHE_KEY, None)?;
		let mut cache = Self::new(enabled, allowlist);
		if !enabled {
			cache.store = Some(store);
			return Ok(cache);
		}
		if let Some(saved) = &saved_routes {
			let now = Utc::now().timestamp() as u64;
			let mut ignored = HashSet::new();
			for item in saved.items.clone() {
				if let RouteRelayItem::Announcement(announcement) = &item {
					if announcement.valid_until <= now || !cache.allowed(announcement.swap_identity)
					{
						ignored.insert(announcement.route_id);
						continue;
					}
				}
				if ignored.contains(&item.route_id()) {
					continue;
				}
				let _ = cache.insert(item, None);
			}
		}
		let route_records = store
			.iter(Some(ROUTE_PREFIX), |key, value| {
				Ok((key.to_vec(), value.to_vec()))
			})?
			.collect::<Result<Vec<_>, _>>()?;
		for (key, value) in route_records {
			if key.len() != 32 {
				continue;
			}
			if let Ok(saved) = ser::deserialize_default::<mwixnet_protocol::MwixnetRoutes, _>(
				&mut value.as_slice(),
			) {
				for item in saved.items {
					let _ = cache.insert(item, None);
				}
			}
		}
		let saved_offers: Option<mwixnet_protocol::MwixnetOffers> =
			store.get_ser(Some(OFFER_PREFIX), OFFER_CACHE_KEY, None)?;
		if let Some(saved) = &saved_offers {
			for item in saved.items.clone() {
				let _ = cache.insert_offer(item, None);
			}
		}
		let offer_records = store
			.iter(Some(OFFER_PREFIX), |key, value| {
				Ok((key.to_vec(), value.to_vec()))
			})?
			.collect::<Result<Vec<_>, _>>()?;
		for (key, value) in offer_records {
			if key.len() != 32 {
				continue;
			}
			if let Ok(saved) = ser::deserialize_default::<mwixnet_protocol::MwixnetOffers, _>(
				&mut value.as_slice(),
			) {
				for item in saved.items {
					let _ = cache.insert_offer(item, None);
				}
			}
		}
		cache.store = Some(store);
		if saved_routes.is_some() || saved_offers.is_some() {
			cache.migrate_legacy_store(saved_routes.is_some(), saved_offers.is_some())?;
		}
		Ok(cache)
	}

	pub fn enabled(&self) -> bool {
		self.enabled
	}

	pub fn snapshot(&self) -> (Vec<RouteRelayItem>, Vec<OfferAnnouncement>) {
		if !self.enabled {
			return (Vec::new(), Vec::new());
		}
		let now = Utc::now().timestamp() as u64;
		let routes = self
			.routes
			.read()
			.values()
			.filter(|group| group.announcement.valid_until > now)
			.flat_map(|group| route_items_at(group, now))
			.collect();
		let offers = self
			.offers
			.read()
			.values()
			.filter(|item| item.offer.valid_until() > now)
			.cloned()
			.collect();
		(routes, offers)
	}

	pub fn insert(
		&self,
		item: RouteRelayItem,
		peer: Option<PeerAddr>,
	) -> Result<bool, RouteCacheError> {
		self.insert_inner(item, peer, None)
	}

	pub fn insert_api(
		&self,
		item: RouteRelayItem,
		caller: PeerAddr,
	) -> Result<bool, RouteCacheError> {
		self.insert_inner(item, None, Some(caller))
	}

	fn insert_inner(
		&self,
		item: RouteRelayItem,
		peer: Option<PeerAddr>,
		api_caller: Option<PeerAddr>,
	) -> Result<bool, RouteCacheError> {
		if !self.enabled {
			return Err(RouteCacheError::Disabled);
		}
		if let Some(caller) = api_caller {
			self.check_api_rate(caller, false, false)?;
		}

		let now = Utc::now().timestamp() as u64;
		let route_id = item.route_id();
		if !matches!(&item, RouteRelayItem::Announcement(_))
			&& !self.routes.read().contains_key(&route_id)
		{
			return Ok(false);
		}
		item.validate(now).map_err(|_| RouteCacheError::Invalid)?;
		let manifest_sequence = item.manifest_sequence();
		let mut routes = self.routes.write();
		self.prune_expired_routes(&mut routes, now)?;

		let new_route = !routes.contains_key(&route_id);
		if new_route {
			if let Some(caller) = api_caller {
				self.check_api_rate(caller, true, false)?;
			}
		}
		let mut evicted = None;
		if new_route {
			if !matches!(&item, RouteRelayItem::Announcement(_)) {
				return Err(RouteCacheError::Invalid);
			}
			if let Some(peer) = peer {
				self.check_new_route_rate(peer)?;
			}
			if routes.len() >= ROUTE_CACHE_LIMIT {
				let candidate_rank = match &item {
					RouteRelayItem::Announcement(item) => {
						(false, item.last_verified, item.route_id)
					}
					_ => unreachable!(),
				};
				let evict = routes
					.iter()
					.min_by_key(|(route_id, group)| route_rank(**route_id, group))
					.map(|(route_id, group)| (*route_id, route_rank(*route_id, group)));
				match evict {
					Some((evict_id, rank)) if candidate_rank > rank => {
						evicted = Some(evict_id);
					}
					_ => return Err(RouteCacheError::RateLimited),
				}
			}
		}

		let mut regular_update = false;
		let mut revocation_update = None;
		let group = match item {
			RouteRelayItem::Announcement(announcement) => {
				if !self.allowed(announcement.swap_identity) {
					return Err(RouteCacheError::Invalid);
				}
				if let Some(current) = routes.get(&route_id) {
					let mut group = current.clone();
					if announcement.swap_identity != group.announcement.swap_identity {
						return Err(RouteCacheError::Invalid);
					}
					if manifest_sequence < group.announcement.manifest_sequence {
						return Ok(false);
					}
					if manifest_sequence == group.announcement.manifest_sequence {
						if announcement == group.announcement {
							return Ok(false);
						}
						let last_sequence = group
							.status
							.as_ref()
							.map(|status| status.sequence)
							.unwrap_or(group.announcement.sequence);
						if announcement.manifest_hash != group.announcement.manifest_hash
							|| announcement.entry_onion != group.announcement.entry_onion
							|| announcement.hop_count != group.announcement.hop_count
							|| announcement.participant_identities
								!= group.announcement.participant_identities
							|| announcement.fee_per_hop != group.announcement.fee_per_hop
						{
							return Err(RouteCacheError::Invalid);
						}
						if announcement.sequence < last_sequence {
							return Ok(false);
						}
						if announcement.sequence == last_sequence {
							return Err(RouteCacheError::Invalid);
						}
						self.check_update_rate(route_id)?;
						regular_update = true;
						group.announcement = announcement.clone();
						group.status = None;
						return self.finish_route_update(
							&mut routes,
							route_id,
							group,
							regular_update,
							None,
							evicted,
						);
					} else {
						self.check_update_rate(route_id)?;
						regular_update = true;
					}
				}
				RouteGroup {
					announcement,
					status: None,
					revocations: HashMap::new(),
				}
			}
			RouteRelayItem::Status(status) => {
				let Some(current) = routes.get(&route_id) else {
					return Ok(false);
				};
				let mut group = current.clone();
				if manifest_sequence != group.announcement.manifest_sequence
					|| status.manifest_hash != group.announcement.manifest_hash
					|| status.swap_identity != group.announcement.swap_identity
					|| status.last_verified != group.announcement.last_verified
					|| status.valid_until > group.announcement.valid_until
				{
					return Err(RouteCacheError::Invalid);
				}
				let last_sequence = group
					.status
					.as_ref()
					.map(|previous| previous.sequence)
					.unwrap_or(group.announcement.sequence);
				if status.sequence <= last_sequence {
					return if group.status.as_ref() == Some(&status) {
						Ok(false)
					} else if status.sequence == last_sequence {
						Err(RouteCacheError::Invalid)
					} else {
						Ok(false)
					};
				}
				self.check_update_rate(route_id)?;
				regular_update = true;
				group.status = Some(status);
				group
			}
			RouteRelayItem::Revocation(revocation) => {
				let Some(current) = routes.get(&route_id) else {
					return Ok(false);
				};
				let mut group = current.clone();
				if manifest_sequence != group.announcement.manifest_sequence
					|| revocation.manifest_hash != group.announcement.manifest_hash
					|| !group
						.announcement
						.participant_identities
						.contains(&revocation.participant_identity)
				{
					return Err(RouteCacheError::Invalid);
				}
				if let Some(previous) = group.revocations.get(&revocation.participant_identity) {
					if revocation.sequence <= previous.sequence {
						return if previous == &revocation {
							Ok(false)
						} else if revocation.sequence == previous.sequence {
							Err(RouteCacheError::Invalid)
						} else {
							Ok(false)
						};
					}
				}
				self.check_revocation_update_rate(route_id, revocation.participant_identity)?;
				revocation_update = Some(revocation.participant_identity);
				group
					.revocations
					.insert(revocation.participant_identity, revocation);
				group
			}
		};
		self.finish_route_update(
			&mut routes,
			route_id,
			group,
			regular_update,
			revocation_update,
			evicted,
		)
	}

	pub fn page(
		&self,
		cursor: Option<Hash>,
		limit: u16,
	) -> Result<(Option<Hash>, Vec<RouteRelayItem>), RouteCacheError> {
		if !self.enabled {
			return Err(RouteCacheError::Disabled);
		}
		if limit == 0 || limit as usize > mwixnet_protocol::P2P_BATCH_MAX_ROUTES {
			return Err(RouteCacheError::Invalid);
		}

		let now = Utc::now().timestamp() as u64;
		let mut routes = self.routes.write();
		self.prune_expired_routes(&mut routes, now)?;
		let selected = routes
			.iter()
			.filter(|(route_id, _)| cursor.map(|cursor| **route_id > cursor).unwrap_or(true))
			.take(limit as usize)
			.collect::<Vec<_>>();
		let has_more = selected
			.last()
			.map(|(route_id, _)| routes.range((**route_id)..).count() > 1)
			.unwrap_or(false);
		let selected_last = selected.last().map(|(route_id, _)| **route_id);
		let mut items = Vec::new();
		let mut bytes = 47usize;
		let mut last_route = None;
		for (route_id, group) in selected {
			let group_items = route_items(group);
			let group_bytes = group_items
				.iter()
				.map(|item| ser::ser_vec(item, ProtocolVersion::local()).map(|bytes| bytes.len()))
				.collect::<Result<Vec<_>, _>>()
				.map_err(|_| RouteCacheError::Invalid)?
				.into_iter()
				.sum::<usize>();
			if bytes + group_bytes > mwixnet_protocol::P2P_BATCH_MAX_BYTES as usize {
				break;
			}
			bytes += group_bytes;
			items.extend(group_items);
			last_route = Some(*route_id);
		}
		let next_cursor = if last_route != selected_last || has_more {
			last_route
		} else {
			None
		};
		Ok((next_cursor, items))
	}

	pub fn page_api(
		&self,
		caller: PeerAddr,
		cursor: Option<Hash>,
		limit: u16,
	) -> Result<(Option<Hash>, Vec<RouteRelayItem>), RouteCacheError> {
		self.check_api_rate(caller, false, false)?;
		self.page(cursor, limit)
	}

	pub fn insert_offer(
		&self,
		item: OfferAnnouncement,
		peer: Option<PeerAddr>,
	) -> Result<bool, RouteCacheError> {
		self.insert_offer_inner(item, peer, None)
	}

	pub fn insert_offer_api(
		&self,
		item: OfferAnnouncement,
		caller: PeerAddr,
	) -> Result<bool, RouteCacheError> {
		self.insert_offer_inner(item, None, Some(caller))
	}

	fn insert_offer_inner(
		&self,
		item: OfferAnnouncement,
		peer: Option<PeerAddr>,
		api_caller: Option<PeerAddr>,
	) -> Result<bool, RouteCacheError> {
		if let Some(caller) = api_caller {
			self.check_api_rate(caller, false, false)?;
		}
		let now = Utc::now().timestamp() as u64;
		item.validate(now).map_err(|_| RouteCacheError::Invalid)?;
		let offer_id = item.offer_id();
		let key = offer_key(&item);
		let mut offers = self.offers.write();
		let expired = offers
			.iter()
			.filter(|(_, item)| item.offer.valid_until() <= now)
			.map(|(offer_id, _)| *offer_id)
			.collect::<Vec<_>>();
		for offer_id in &expired {
			offers.remove(offer_id);
		}
		self.delete_offers(&expired)?;
		self.prune_offer_metadata(&offers);
		let previous = offers
			.iter()
			.find(|(_, previous)| offer_key(previous) == key)
			.map(|(id, previous)| (*id, previous.clone()));
		let new_offer = previous.is_none();
		if new_offer {
			if let Some(caller) = api_caller {
				self.check_api_rate(caller, false, true)?;
			}
		}
		if let Some((_, previous)) = &previous {
			if item.offer.sequence() < previous.offer.sequence() {
				return Ok(false);
			}
			if item.offer.sequence() == previous.offer.sequence() {
				return if item.offer == previous.offer {
					Ok(false)
				} else {
					Err(RouteCacheError::Invalid)
				};
			}
			self.check_offer_update_rate(key)?;
		} else if let Some(peer) = peer {
			self.check_new_offer_rate(peer)?;
		}
		let evicted = if offers.len() >= OFFER_CACHE_LIMIT && new_offer {
			let candidate_rank = (item.offer.valid_until(), offer_id);
			let evict = offers
				.iter()
				.min_by_key(|(id, item)| (item.offer.valid_until(), **id))
				.map(|(id, item)| (*id, (item.offer.valid_until(), *id)));
			match evict {
				Some((evict_id, rank)) if candidate_rank > rank => Some(evict_id),
				_ => return Err(RouteCacheError::RateLimited),
			}
		} else {
			None
		};
		let mut removed = expired;
		if let Some((previous_id, _)) = previous {
			removed.push(previous_id);
		}
		if let Some(evict_id) = evicted {
			removed.push(evict_id);
		}
		self.persist_offer(offer_id, &item, &removed)?;
		for removed_id in removed {
			offers.remove(&removed_id);
		}
		offers.insert(offer_id, item);
		if !new_offer {
			self.offer_updates
				.write()
				.insert(key, Utc::now().timestamp() / 60);
		}
		Ok(true)
	}

	pub fn offer_page(
		&self,
		cursor: Option<Hash>,
		limit: u16,
	) -> Result<(Option<Hash>, Vec<OfferAnnouncement>), RouteCacheError> {
		if limit == 0 || limit as usize > mwixnet_protocol::P2P_OFFER_BATCH_MAX_ITEMS {
			return Err(RouteCacheError::Invalid);
		}
		let now = Utc::now().timestamp() as u64;
		let mut offers = self.offers.write();
		let expired = offers
			.iter()
			.filter(|(_, item)| item.offer.valid_until() <= now)
			.map(|(offer_id, _)| *offer_id)
			.collect::<Vec<_>>();
		for offer_id in &expired {
			offers.remove(offer_id);
		}
		self.delete_offers(&expired)?;
		self.prune_offer_metadata(&offers);
		let mut items = Vec::new();
		let mut bytes = 47usize;
		let mut last = None;
		let mut more = false;
		for (id, item) in offers
			.iter()
			.filter(|(id, _)| cursor.map(|cursor| **id > cursor).unwrap_or(true))
		{
			if items.len() == limit as usize {
				more = true;
				break;
			}
			let item_bytes = ser::ser_vec(item, ProtocolVersion::local())
				.map_err(|_| RouteCacheError::Invalid)?
				.len();
			if bytes + item_bytes > mwixnet_protocol::P2P_OFFER_BATCH_MAX_BYTES as usize {
				more = true;
				break;
			}
			bytes += item_bytes;
			items.push(item.clone());
			last = Some(*id);
		}
		Ok((if more { last } else { None }, items))
	}

	pub fn offer_page_api(
		&self,
		caller: PeerAddr,
		cursor: Option<Hash>,
		limit: u16,
	) -> Result<(Option<Hash>, Vec<OfferAnnouncement>), RouteCacheError> {
		self.check_api_rate(caller, false, false)?;
		self.offer_page(cursor, limit)
	}

	fn allowed(&self, identity: PublicKey) -> bool {
		self.allowlist.is_empty() || self.allowlist.contains(&identity)
	}

	fn check_new_route_rate(&self, peer: PeerAddr) -> Result<(), RouteCacheError> {
		let minute = Utc::now().timestamp() / 60;
		let mut rates = self.peer_rates.write();
		let rate = rates.entry(peer).or_insert_with(|| PeerRate::new(minute));
		rate.reset(minute);
		if rate.new_routes >= NEW_ROUTES_PER_MINUTE {
			return Err(RouteCacheError::RateLimited);
		}
		rate.new_routes += 1;
		Ok(())
	}

	fn check_new_offer_rate(&self, peer: PeerAddr) -> Result<(), RouteCacheError> {
		let minute = Utc::now().timestamp() / 60;
		let mut rates = self.peer_rates.write();
		let rate = rates.entry(peer).or_insert_with(|| PeerRate::new(minute));
		rate.reset(minute);
		if rate.new_offers >= NEW_OFFERS_PER_MINUTE {
			return Err(RouteCacheError::RateLimited);
		}
		rate.new_offers += 1;
		Ok(())
	}

	fn check_offer_update_rate(&self, key: (u8, PublicKey)) -> Result<(), RouteCacheError> {
		let minute = Utc::now().timestamp() / 60;
		if self.offer_updates.read().get(&key) == Some(&minute) {
			return Err(RouteCacheError::RateLimited);
		}
		Ok(())
	}

	fn check_update_rate(&self, route_id: Hash) -> Result<(), RouteCacheError> {
		let minute = Utc::now().timestamp() / 60;
		let updates = self.updates.read();
		if updates.get(&route_id) == Some(&minute) {
			return Err(RouteCacheError::RateLimited);
		}
		Ok(())
	}

	fn check_revocation_update_rate(
		&self,
		route_id: Hash,
		participant: PublicKey,
	) -> Result<(), RouteCacheError> {
		let minute = Utc::now().timestamp() / 60;
		if self.revocation_updates.read().get(&(route_id, participant)) == Some(&minute) {
			return Err(RouteCacheError::RateLimited);
		}
		Ok(())
	}

	fn check_api_rate(
		&self,
		caller: PeerAddr,
		new_route: bool,
		new_offer: bool,
	) -> Result<(), RouteCacheError> {
		let minute = Utc::now().timestamp() / 60;
		let mut rates = self.api_rates.write();
		rates.retain(|_, rate| rate.minute >= minute - 1);
		let rate = rates.entry(caller).or_insert(ApiRate {
			minute,
			messages: 0,
			new_routes: 0,
			new_offers: 0,
		});
		if rate.minute != minute {
			*rate = ApiRate {
				minute,
				messages: 0,
				new_routes: 0,
				new_offers: 0,
			};
		}
		let message = !new_route && !new_offer;
		if (message && rate.messages >= NEW_MESSAGES_PER_MINUTE)
			|| (new_route && rate.new_routes >= NEW_ROUTES_PER_MINUTE)
			|| (new_offer && rate.new_offers >= NEW_OFFERS_PER_MINUTE)
		{
			return Err(RouteCacheError::RateLimited);
		}
		if message {
			rate.messages += 1;
		}
		if new_route {
			rate.new_routes += 1;
		}
		if new_offer {
			rate.new_offers += 1;
		}
		Ok(())
	}

	fn prune_expired_routes(
		&self,
		routes: &mut BTreeMap<Hash, RouteGroup>,
		now: u64,
	) -> Result<(), RouteCacheError> {
		let expired = routes
			.iter()
			.filter(|(_, group)| group.announcement.valid_until <= now)
			.map(|(route_id, _)| *route_id)
			.collect::<Vec<_>>();
		for route_id in &expired {
			routes.remove(route_id);
		}
		self.delete_routes(&expired)?;

		let expired_statuses = routes
			.iter()
			.filter(|(_, group)| {
				group
					.status
					.as_ref()
					.map(|status| status.valid_until <= now)
					.unwrap_or(false)
			})
			.map(|(route_id, _)| *route_id)
			.collect::<Vec<_>>();
		for route_id in expired_statuses {
			if let Some(mut group) = routes.get(&route_id).cloned() {
				group.status = None;
				self.persist_route(route_id, &group, &[])?;
				routes.insert(route_id, group);
			}
		}
		self.prune_route_metadata(routes);
		Ok(())
	}

	fn finish_route_update(
		&self,
		routes: &mut BTreeMap<Hash, RouteGroup>,
		route_id: Hash,
		group: RouteGroup,
		regular_update: bool,
		revocation_update: Option<PublicKey>,
		evicted: Option<Hash>,
	) -> Result<bool, RouteCacheError> {
		let removed = evicted.into_iter().collect::<Vec<_>>();
		self.persist_route(route_id, &group, &removed)?;
		for route_id in removed {
			routes.remove(&route_id);
		}
		routes.insert(route_id, group);
		let minute = Utc::now().timestamp() / 60;
		if regular_update {
			self.updates.write().insert(route_id, minute);
		}
		if let Some(participant) = revocation_update {
			self.revocation_updates
				.write()
				.insert((route_id, participant), minute);
		}
		self.prune_route_metadata(routes);
		Ok(true)
	}

	fn persist_route(
		&self,
		route_id: Hash,
		group: &RouteGroup,
		removed: &[Hash],
	) -> Result<(), RouteCacheError> {
		let Some(store) = &self.store else {
			return Ok(());
		};
		let saved = mwixnet_protocol::MwixnetRoutes {
			version: mwixnet_protocol::MWIXNET_PROTOCOL_VERSION,
			request_id: 1,
			next_cursor: None,
			items: route_items(group),
		};
		let mut batch = store.batch().map_err(|_| RouteCacheError::Store)?;
		for removed_id in removed {
			batch
				.delete(Some(ROUTE_PREFIX), removed_id.as_bytes())
				.map_err(|_| RouteCacheError::Store)?;
		}
		batch
			.put_ser(Some(ROUTE_PREFIX), route_id.as_bytes(), &saved)
			.map_err(|_| RouteCacheError::Store)?;
		batch.commit().map_err(|_| RouteCacheError::Store)
	}

	fn persist_offer(
		&self,
		offer_id: Hash,
		offer: &OfferAnnouncement,
		removed: &[Hash],
	) -> Result<(), RouteCacheError> {
		let Some(store) = &self.store else {
			return Ok(());
		};
		let saved = mwixnet_protocol::MwixnetOffers {
			version: mwixnet_protocol::MWIXNET_PROTOCOL_VERSION,
			request_id: 1,
			next_cursor: None,
			items: vec![offer.clone()],
		};
		let mut batch = store.batch().map_err(|_| RouteCacheError::Store)?;
		for removed_id in removed {
			batch
				.delete(Some(OFFER_PREFIX), removed_id.as_bytes())
				.map_err(|_| RouteCacheError::Store)?;
		}
		batch
			.put_ser(Some(OFFER_PREFIX), offer_id.as_bytes(), &saved)
			.map_err(|_| RouteCacheError::Store)?;
		batch.commit().map_err(|_| RouteCacheError::Store)
	}

	fn delete_routes(&self, route_ids: &[Hash]) -> Result<(), RouteCacheError> {
		let Some(store) = &self.store else {
			return Ok(());
		};
		if route_ids.is_empty() {
			return Ok(());
		}
		let mut batch = store.batch().map_err(|_| RouteCacheError::Store)?;
		for route_id in route_ids {
			batch
				.delete(Some(ROUTE_PREFIX), route_id.as_bytes())
				.map_err(|_| RouteCacheError::Store)?;
		}
		batch.commit().map_err(|_| RouteCacheError::Store)
	}

	fn delete_offers(&self, offer_ids: &[Hash]) -> Result<(), RouteCacheError> {
		let Some(store) = &self.store else {
			return Ok(());
		};
		if offer_ids.is_empty() {
			return Ok(());
		}
		let mut batch = store.batch().map_err(|_| RouteCacheError::Store)?;
		for offer_id in offer_ids {
			batch
				.delete(Some(OFFER_PREFIX), offer_id.as_bytes())
				.map_err(|_| RouteCacheError::Store)?;
		}
		batch.commit().map_err(|_| RouteCacheError::Store)
	}

	fn prune_route_metadata(&self, routes: &BTreeMap<Hash, RouteGroup>) {
		self.updates
			.write()
			.retain(|route_id, _| routes.contains_key(route_id));
		self.revocation_updates
			.write()
			.retain(|(route_id, _), _| routes.contains_key(route_id));
		let minute = Utc::now().timestamp() / 60;
		self.peer_rates
			.write()
			.retain(|_, rate| rate.minute >= minute - 1);
	}

	fn prune_offer_metadata(&self, offers: &BTreeMap<Hash, OfferAnnouncement>) {
		let keys = offers.values().map(offer_key).collect::<HashSet<_>>();
		self.offer_updates
			.write()
			.retain(|key, _| keys.contains(key));
		let minute = Utc::now().timestamp() / 60;
		self.peer_rates
			.write()
			.retain(|_, rate| rate.minute >= minute - 1);
	}

	fn migrate_legacy_store(&self, routes: bool, offers: bool) -> Result<(), grin_store::Error> {
		let store = self.store.as_ref().unwrap();
		let mut batch = store.batch()?;
		if routes {
			for (route_id, group) in self.routes.read().iter() {
				let saved = mwixnet_protocol::MwixnetRoutes {
					version: mwixnet_protocol::MWIXNET_PROTOCOL_VERSION,
					request_id: 1,
					next_cursor: None,
					items: route_items(group),
				};
				batch.put_ser(Some(ROUTE_PREFIX), route_id.as_bytes(), &saved)?;
			}
			batch.delete(Some(ROUTE_PREFIX), ROUTE_CACHE_KEY)?;
		}
		if offers {
			for (offer_id, offer) in self.offers.read().iter() {
				let saved = mwixnet_protocol::MwixnetOffers {
					version: mwixnet_protocol::MWIXNET_PROTOCOL_VERSION,
					request_id: 1,
					next_cursor: None,
					items: vec![offer.clone()],
				};
				batch.put_ser(Some(OFFER_PREFIX), offer_id.as_bytes(), &saved)?;
			}
			batch.delete(Some(OFFER_PREFIX), OFFER_CACHE_KEY)?;
		}
		batch.commit()
	}
}

fn route_items(group: &RouteGroup) -> Vec<RouteRelayItem> {
	route_items_at(group, 0)
}

fn route_items_at(group: &RouteGroup, now: u64) -> Vec<RouteRelayItem> {
	let mut items = vec![RouteRelayItem::Announcement(group.announcement.clone())];
	if let Some(status) = group
		.status
		.as_ref()
		.filter(|status| status.valid_until > now)
	{
		items.push(RouteRelayItem::Status(status.clone()));
	}
	let mut revocations = group.revocations.values().cloned().collect::<Vec<_>>();
	revocations.sort_by_key(|item| item.participant_identity.0);
	items.extend(revocations.into_iter().map(RouteRelayItem::Revocation));
	items
}

fn offer_key(item: &OfferAnnouncement) -> (u8, PublicKey) {
	match &item.offer {
		mwixnet_protocol::MwixnetOffer::Mixer(offer) => (0, offer.identity_public_key),
		mwixnet_protocol::MwixnetOffer::Swap(offer) => (1, offer.identity_public_key),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ed25519_dalek::{Signer, SigningKey};
	use mwixnet_protocol::{
		MixerOffer, MwixnetOffer, MwixnetType, OnionAddress, OnionPublicKey, RouteAnnouncement,
		RouteRevocation, RouteState, RouteStatus, Signature, MWIXNET_PROTOCOL_VERSION,
	};

	fn announcement(key: &SigningKey, now: u64) -> RouteAnnouncement {
		let identity = PublicKey(key.verifying_key().to_bytes());
		let mut item = RouteAnnouncement {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::RouteAnnouncement,
			route_id: Hash([1; 32]),
			manifest_sequence: 1,
			entry_onion: OnionAddress(identity.0),
			swap_identity: identity,
			hop_count: 2,
			participant_identities: vec![
				identity,
				PublicKey(SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes()),
			],
			fee_per_hop: 1,
			manifest_hash: Hash([2; 32]),
			health_hash: Hash([4; 32]),
			status: RouteState::Healthy,
			last_verified: now,
			valid_until: now + 600,
			sequence: 1,
			signature: Signature([0; 64]),
		};
		item.signature = Signature(key.sign(item.hash().as_bytes()).to_bytes());
		item
	}

	fn offer(key: &SigningKey, now: u64) -> OfferAnnouncement {
		let identity = PublicKey(key.verifying_key().to_bytes());
		let mut offer = MixerOffer {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::MixerOffer,
			identity_public_key: identity,
			onion_address: OnionAddress(identity.0),
			onion_public_key: OnionPublicKey([2; 32]),
			minimum_fee: 1,
			capacity: 32,
			valid_until: now + 3_600,
			sequence: 1,
			signature: Signature([0; 64]),
		};
		offer.signature = Signature(key.sign(offer.hash().as_bytes()).to_bytes());
		OfferAnnouncement::mine(MwixnetOffer::Mixer(offer))
	}

	#[test]
	fn validates_and_pages_offers() {
		let now = Utc::now().timestamp() as u64;
		let item = offer(&SigningKey::from_bytes(&[7; 32]), now);
		let cache = RouteCache::new(false, vec![]);
		assert_eq!(cache.insert_offer(item.clone(), None), Ok(true));
		assert_eq!(cache.insert_offer(item.clone(), None), Ok(false));
		let (_, items) = cache.offer_page(None, 1).unwrap();
		assert_eq!(items, vec![item.clone()]);

		let mut invalid = item;
		invalid.pow_nonce = invalid.pow_nonce.wrapping_add(1);
		assert_eq!(
			cache.insert_offer(invalid, None),
			Err(RouteCacheError::Invalid)
		);
	}

	#[test]
	fn validates_and_updates_routes() {
		let now = Utc::now().timestamp() as u64;
		let key = SigningKey::from_bytes(&[7; 32]);
		let announcement = announcement(&key, now);
		let cache = RouteCache::new(true, vec![]);

		assert_eq!(
			cache.insert(RouteRelayItem::Announcement(announcement.clone()), None),
			Ok(true)
		);
		assert_eq!(
			cache.insert(RouteRelayItem::Announcement(announcement.clone()), None),
			Ok(false)
		);

		let mut status = RouteStatus {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::RouteStatus,
			route_id: announcement.route_id,
			manifest_sequence: announcement.manifest_sequence,
			manifest_hash: announcement.manifest_hash,
			status: RouteState::Draining,
			last_verified: now,
			valid_until: now + 600,
			sequence: 2,
			swap_identity: announcement.swap_identity,
			signature: Signature([0; 64]),
		};
		status.signature = Signature(key.sign(status.hash().as_bytes()).to_bytes());
		assert_eq!(
			cache.insert(RouteRelayItem::Status(status.clone()), None),
			Ok(true)
		);
		let (_, items) = cache.page(None, 1).unwrap();
		assert_eq!(
			items,
			vec![
				RouteRelayItem::Announcement(announcement),
				RouteRelayItem::Status(status.clone()),
			]
		);

		status.status = RouteState::Unavailable;
		status.signature = Signature(key.sign(status.hash().as_bytes()).to_bytes());
		assert_eq!(
			cache.insert(RouteRelayItem::Status(status), None),
			Err(RouteCacheError::Invalid)
		);
	}

	#[test]
	fn drops_expired_status_while_announcement_is_valid() {
		let now = Utc::now().timestamp() as u64;
		let key = SigningKey::from_bytes(&[7; 32]);
		let announcement = announcement(&key, now);
		let mut status = RouteStatus {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::RouteStatus,
			route_id: announcement.route_id,
			manifest_sequence: announcement.manifest_sequence,
			manifest_hash: announcement.manifest_hash,
			status: RouteState::Draining,
			last_verified: now - 1,
			valid_until: now,
			sequence: 2,
			swap_identity: announcement.swap_identity,
			signature: Signature([0; 64]),
		};
		status.signature = Signature(key.sign(status.hash().as_bytes()).to_bytes());
		let cache = RouteCache::new(true, vec![]);
		cache.routes.write().insert(
			announcement.route_id,
			RouteGroup {
				announcement: announcement.clone(),
				status: Some(status),
				revocations: HashMap::new(),
			},
		);

		assert_eq!(
			cache.page(None, 1).unwrap().1,
			vec![RouteRelayItem::Announcement(announcement.clone())]
		);
		assert_eq!(
			cache.snapshot().0,
			vec![RouteRelayItem::Announcement(announcement)]
		);
		assert!(cache
			.routes
			.read()
			.values()
			.next()
			.unwrap()
			.status
			.is_none());
	}

	#[test]
	fn requires_an_allowed_swap_identity() {
		let now = Utc::now().timestamp() as u64;
		let key = SigningKey::from_bytes(&[7; 32]);
		let announcement = announcement(&key, now);
		let cache = RouteCache::new(true, vec![announcement.swap_identity]);
		assert_eq!(
			cache.insert(RouteRelayItem::Announcement(announcement.clone()), None),
			Ok(true)
		);
		let cache = RouteCache::new(true, vec![PublicKey([9; 32])]);
		assert_eq!(
			cache.insert(RouteRelayItem::Announcement(announcement), None),
			Err(RouteCacheError::Invalid)
		);
		let cache = RouteCache::new(false, vec![]);
		assert_eq!(cache.page(None, 1), Err(RouteCacheError::Disabled));
	}

	#[test]
	fn limits_regular_updates() {
		let now = Utc::now().timestamp() as u64;
		let key = SigningKey::from_bytes(&[7; 32]);
		let announcement = announcement(&key, now);
		let cache = RouteCache::new(true, vec![]);
		cache
			.insert(RouteRelayItem::Announcement(announcement.clone()), None)
			.unwrap();
		let mut status = RouteStatus {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::RouteStatus,
			route_id: announcement.route_id,
			manifest_sequence: announcement.manifest_sequence,
			manifest_hash: announcement.manifest_hash,
			status: RouteState::Degraded,
			last_verified: now,
			valid_until: now + 600,
			sequence: 2,
			swap_identity: announcement.swap_identity,
			signature: Signature([0; 64]),
		};
		status.signature = Signature(key.sign(status.hash().as_bytes()).to_bytes());
		cache
			.insert(RouteRelayItem::Status(status.clone()), None)
			.unwrap();
		status.sequence = 3;
		status.signature = Signature(key.sign(status.hash().as_bytes()).to_bytes());
		assert_eq!(
			cache.insert(RouteRelayItem::Status(status), None),
			Err(RouteCacheError::RateLimited)
		);
	}

	#[test]
	fn limits_new_routes_and_offers_per_peer() {
		let cache = RouteCache::new(true, vec![]);
		let peer = PeerAddr("127.0.0.1:3414".parse().unwrap());
		for _ in 0..NEW_ROUTES_PER_MINUTE {
			assert_eq!(cache.check_new_route_rate(peer), Ok(()));
		}
		assert_eq!(
			cache.check_new_route_rate(peer),
			Err(RouteCacheError::RateLimited)
		);
		for _ in 0..NEW_OFFERS_PER_MINUTE {
			assert_eq!(cache.check_new_offer_rate(peer), Ok(()));
		}
		assert_eq!(
			cache.check_new_offer_rate(peer),
			Err(RouteCacheError::RateLimited)
		);
	}

	#[test]
	fn limits_api_messages() {
		let cache = RouteCache::new(true, vec![]);
		let first = PeerAddr("127.0.0.1:3414".parse().unwrap());
		let second = PeerAddr("127.0.0.2:3414".parse().unwrap());
		for _ in 0..NEW_MESSAGES_PER_MINUTE {
			assert_eq!(cache.check_api_rate(first, false, false), Ok(()));
		}
		assert_eq!(
			cache.check_api_rate(first, false, false),
			Err(RouteCacheError::RateLimited)
		);
		assert_eq!(cache.check_api_rate(second, false, false), Ok(()));
	}

	#[test]
	fn limits_invalid_api_messages_before_validation() {
		let now = Utc::now().timestamp() as u64;
		let key = SigningKey::from_bytes(&[7; 32]);
		let mut announcement = announcement(&key, now);
		announcement.signature = Signature([0; 64]);
		let route_caller = PeerAddr("127.0.0.3:3414".parse().unwrap());
		let route_cache = RouteCache::new(true, vec![]);
		for _ in 0..NEW_MESSAGES_PER_MINUTE {
			assert_eq!(
				route_cache.insert_api(
					RouteRelayItem::Announcement(announcement.clone()),
					route_caller,
				),
				Err(RouteCacheError::Invalid)
			);
		}
		assert_eq!(
			route_cache.insert_api(RouteRelayItem::Announcement(announcement), route_caller),
			Err(RouteCacheError::RateLimited)
		);

		let mut offer = offer(&key, now);
		offer.pow_nonce = offer.pow_nonce.wrapping_add(1);
		let offer_caller = PeerAddr("127.0.0.4:3414".parse().unwrap());
		let offer_cache = RouteCache::new(true, vec![]);
		for _ in 0..NEW_MESSAGES_PER_MINUTE {
			assert_eq!(
				offer_cache.insert_offer_api(offer.clone(), offer_caller),
				Err(RouteCacheError::Invalid)
			);
		}
		assert_eq!(
			offer_cache.insert_offer_api(offer, offer_caller),
			Err(RouteCacheError::RateLimited)
		);
	}

	#[test]
	fn limits_revocation_updates_separately() {
		let now = Utc::now().timestamp() as u64;
		let swap_key = SigningKey::from_bytes(&[7; 32]);
		let participant_key = SigningKey::from_bytes(&[8; 32]);
		let announcement = announcement(&swap_key, now);
		let cache = RouteCache::new(true, vec![]);
		cache
			.insert(RouteRelayItem::Announcement(announcement.clone()), None)
			.unwrap();
		let mut revocation = RouteRevocation {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::RouteRevocation,
			route_id: announcement.route_id,
			manifest_sequence: announcement.manifest_sequence,
			manifest_hash: announcement.manifest_hash,
			participant_identity: PublicKey(participant_key.verifying_key().to_bytes()),
			revoked_at: now,
			sequence: 1,
			signature: Signature([0; 64]),
		};
		revocation.signature = Signature(
			participant_key
				.sign(revocation.hash().as_bytes())
				.to_bytes(),
		);
		cache
			.insert(RouteRelayItem::Revocation(revocation.clone()), None)
			.unwrap();

		revocation.sequence = 2;
		revocation.signature = Signature(
			participant_key
				.sign(revocation.hash().as_bytes())
				.to_bytes(),
		);
		assert_eq!(
			cache.insert(RouteRelayItem::Revocation(revocation), None),
			Err(RouteCacheError::RateLimited)
		);
	}

	#[test]
	fn ignores_status_for_unknown_route() {
		let now = Utc::now().timestamp() as u64;
		let key = SigningKey::from_bytes(&[7; 32]);
		let announcement = announcement(&key, now);
		let mut status = RouteStatus {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::RouteStatus,
			route_id: announcement.route_id,
			manifest_sequence: announcement.manifest_sequence,
			manifest_hash: announcement.manifest_hash,
			status: RouteState::Draining,
			last_verified: now,
			valid_until: now + 600,
			sequence: 2,
			swap_identity: announcement.swap_identity,
			signature: Signature([0; 64]),
		};
		status.signature = Signature(key.sign(status.hash().as_bytes()).to_bytes());
		let cache = RouteCache::new(true, vec![]);
		assert_eq!(
			cache.insert(RouteRelayItem::Status(status), None),
			Ok(false)
		);
	}

	#[test]
	fn persists_routes() {
		crate::core::global::set_local_chain_type(
			crate::core::global::ChainTypes::AutomatedTesting,
		);
		let now = Utc::now().timestamp() as u64;
		let key = SigningKey::from_bytes(&[7; 32]);
		let announcement = announcement(&key, now);
		let offer = offer(&key, now);
		let root = std::env::temp_dir().join(format!(
			"grin-mwixnet-routes-{}-{}",
			std::process::id(),
			now
		));
		let _ = std::fs::remove_dir_all(&root);
		{
			let cache = RouteCache::open(true, vec![], root.to_str().unwrap()).unwrap();
			cache
				.insert(RouteRelayItem::Announcement(announcement.clone()), None)
				.unwrap();
			cache.insert_offer(offer.clone(), None).unwrap();
		}
		let mut expired_status = RouteStatus {
			version: MWIXNET_PROTOCOL_VERSION,
			msg_type: MwixnetType::RouteStatus,
			route_id: announcement.route_id,
			manifest_sequence: announcement.manifest_sequence,
			manifest_hash: announcement.manifest_hash,
			status: RouteState::Draining,
			last_verified: now - 2,
			valid_until: now - 1,
			sequence: 2,
			swap_identity: announcement.swap_identity,
			signature: Signature([0; 64]),
		};
		expired_status.signature = Signature(key.sign(expired_status.hash().as_bytes()).to_bytes());
		let store = grin_store::Store::new(
			root.to_str().unwrap(),
			Some("mwixnet"),
			Some("routes"),
			vec![ROUTE_PREFIX, OFFER_PREFIX],
			None,
			None,
		)
		.unwrap();
		let saved = mwixnet_protocol::MwixnetRoutes {
			version: MWIXNET_PROTOCOL_VERSION,
			request_id: 1,
			next_cursor: None,
			items: vec![
				RouteRelayItem::Announcement(announcement.clone()),
				RouteRelayItem::Status(expired_status),
			],
		};
		let mut batch = store.batch().unwrap();
		batch
			.delete(Some(ROUTE_PREFIX), announcement.route_id.as_bytes())
			.unwrap();
		batch
			.put_ser(Some(ROUTE_PREFIX), ROUTE_CACHE_KEY, &saved)
			.unwrap();
		batch.commit().unwrap();
		drop(store);
		let disabled = RouteCache::open(false, vec![], root.to_str().unwrap()).unwrap();
		assert_eq!(disabled.page(None, 1), Err(RouteCacheError::Disabled));
		drop(disabled);
		let cache = RouteCache::open(true, vec![], root.to_str().unwrap()).unwrap();
		let (_, items) = cache.page(None, 1).unwrap();
		assert_eq!(items, vec![RouteRelayItem::Announcement(announcement)]);
		let (_, items) = cache.offer_page(None, 1).unwrap();
		assert_eq!(items, vec![offer]);
		std::fs::remove_dir_all(root).unwrap();
	}
}

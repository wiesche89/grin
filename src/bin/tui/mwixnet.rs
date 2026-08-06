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

//! MWixnet route display

use crate::servers::ServerStats;
use crate::tui::constants::{MAIN_MENU, MWIXNET_ROUTES, MWIXNET_SCROLL, VIEW_MWIXNET};
use crate::tui::types::TUIStatusListener;
use chrono::{TimeZone, Utc};
use cursive::event::Key;
use cursive::traits::{Nameable, Resizable, Scrollable};
use cursive::views::{Dialog, OnEventView, TextView};
use cursive::Cursive;
use grin_p2p::mwixnet_protocol::{MwixnetOffer, RouteRelayItem, RouteState};

pub struct TUIMwixnetView;

fn format_timestamp(timestamp: u64) -> String {
	Utc.timestamp_opt(timestamp as i64, 0)
		.single()
		.map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
		.unwrap_or_else(|| timestamp.to_string())
}

impl TUIMwixnetView {
	pub fn create() -> impl cursive::view::View {
		let view = Dialog::around(
			TextView::new("No MWixnet discovery data in cache.")
				.with_name(MWIXNET_ROUTES)
				.scrollable()
				.with_name(MWIXNET_SCROLL)
				.full_screen(),
		)
		.title("MWixnet Discovery")
		.with_name(VIEW_MWIXNET);

		OnEventView::new(view).on_pre_event(Key::Esc, move |c| {
			let _ = c.focus_name(MAIN_MENU);
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn mwixnet_scroll_view_can_be_focused() {
		let mut c = Cursive::new();
		c.add_layer(TUIMwixnetView::create());
		assert!(c.focus_name(MWIXNET_SCROLL).is_ok());
	}
}

impl TUIStatusListener for TUIMwixnetView {
	fn update(c: &mut Cursive, stats: &ServerStats) {
		let routes = stats
			.mwixnet_routes
			.iter()
			.filter_map(|item| match item {
				RouteRelayItem::Announcement(route) => Some(route),
				_ => None,
			})
			.collect::<Vec<_>>();
		let mut lines = if stats.mwixnet_route_relay {
			vec![format!("Routes: {}", routes.len()), String::new()]
		} else {
			vec!["Route relay: disabled".to_string(), String::new()]
		};
		for (index, route) in routes.iter().enumerate() {
			let status = stats
				.mwixnet_routes
				.iter()
				.filter_map(|item| match item {
					RouteRelayItem::Status(status) if status.route_id == route.route_id => {
						Some(status)
					}
					_ => None,
				})
				.max_by_key(|status| status.sequence);
			let revocations = stats
				.mwixnet_routes
				.iter()
				.filter(
					|item| matches!(item, RouteRelayItem::Revocation(item) if item.route_id == route.route_id),
				)
				.count();
			let state = if revocations > 0 {
				RouteState::Revoked
			} else {
				status.map(|status| status.status).unwrap_or(route.status)
			};
			let last_verified = status
				.map(|status| status.last_verified)
				.unwrap_or(route.last_verified);
			let valid_until = status
				.map(|status| status.valid_until)
				.unwrap_or(route.valid_until);
			let total_fee = route.fee_per_hop.saturating_mul(route.hop_count as u64);

			lines.extend([
				format!("{}. Route health: {:?}", index + 1, state),
				format!("Route: {:?}", route.route_id),
				format!("Entry: {}", route.entry_onion),
				"Entry preflight: not checked by node".to_string(),
				format!("Hops: {}", route.hop_count),
				format!(
					"Fee: {:.9} GRIN per hop, {:.9} total",
					route.fee_per_hop as f64 / 1_000_000_000.0,
					total_fee as f64 / 1_000_000_000.0,
				),
				format!("Verified: {}", format_timestamp(last_verified)),
				format!("Valid until: {}", format_timestamp(valid_until)),
			]);
			if revocations > 0 {
				lines.push(format!("Revocations: {}", revocations));
			}
			lines.push(String::new());
		}

		lines.push(format!("Offers: {}", stats.mwixnet_offers.len()));
		lines.push(String::new());
		for (index, announcement) in stats.mwixnet_offers.iter().enumerate() {
			let (kind, identity, onion, minimum_fee, capacity, valid_until, sequence) =
				match &announcement.offer {
					MwixnetOffer::Mixer(offer) => (
						"Mixer",
						offer.identity_public_key,
						offer.onion_address,
						offer.minimum_fee,
						offer.capacity,
						offer.valid_until,
						offer.sequence,
					),
					MwixnetOffer::Swap(offer) => (
						"Swap",
						offer.identity_public_key,
						offer.onion_address,
						offer.minimum_fee,
						offer.capacity,
						offer.valid_until,
						offer.sequence,
					),
				};
			lines.extend([
				format!("{}. {}", index + 1, kind),
				format!("Identity: {:?}", identity),
				format!("Onion: {}", onion),
				format!(
					"Minimum fee: {:.9} GRIN",
					minimum_fee as f64 / 1_000_000_000.0
				),
				format!("Capacity: {}", capacity),
				format!("Sequence: {}", sequence),
				format!("Valid until: {}", format_timestamp(valid_until)),
				String::new(),
			]);
		}
		let content = lines.join("\n");
		let _ = c.call_on_name(MWIXNET_ROUTES, |view: &mut TextView| {
			view.set_content(content);
		});
	}
}

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

use grin_core::ser::{self, ProtocolVersion};
use grin_p2p::mwixnet_protocol::{
	GetMwixnetRoutes, Hash, MwixnetRoutes, RouteAnnouncement, RouteRelayItem, RouteRevocation,
	RouteStatus, MWIXNET_PROTOCOL_VERSION,
};
use grin_util::ToHex;
use serde_derive::Deserialize;

#[derive(Deserialize)]
struct SignedVector<T> {
	value: T,
	binary: String,
	hash: String,
}

#[derive(Deserialize)]
struct Vectors {
	announcement: SignedVector<RouteAnnouncement>,
	status: SignedVector<RouteStatus>,
	revocation: SignedVector<RouteRevocation>,
	get_routes_binary: String,
	routes_binary: String,
}

fn binary<T: grin_core::ser::Writeable>(value: &T) -> String {
	ser::ser_vec(value, ProtocolVersion::local())
		.unwrap()
		.to_hex()
}

#[test]
fn mwixnet_discovery_vectors() {
	let vectors: Vectors = serde_json::from_str(include_str!("mwixnet_vectors.json")).unwrap();
	let announcement = vectors.announcement.value;
	let status = vectors.status.value;
	let revocation = vectors.revocation.value;
	let get = GetMwixnetRoutes {
		version: MWIXNET_PROTOCOL_VERSION,
		request_id: 9,
		cursor: Some(Hash([4; 32])),
		limit: 10,
	};
	let routes = MwixnetRoutes {
		version: MWIXNET_PROTOCOL_VERSION,
		request_id: 9,
		next_cursor: None,
		items: vec![
			RouteRelayItem::Announcement(announcement.clone()),
			RouteRelayItem::Status(status.clone()),
			RouteRelayItem::Revocation(revocation.clone()),
		],
	};

	assert_eq!(vectors.announcement.binary, binary(&announcement));
	assert_eq!(vectors.announcement.hash, announcement.hash().0.to_hex());
	assert_eq!(vectors.status.binary, binary(&status));
	assert_eq!(vectors.status.hash, status.hash().0.to_hex());
	assert_eq!(vectors.revocation.binary, binary(&revocation));
	assert_eq!(vectors.revocation.hash, revocation.hash().0.to_hex());
	assert_eq!(vectors.get_routes_binary, binary(&get));
	assert_eq!(vectors.routes_binary, binary(&routes));
}

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

use grin_core as core;
use grin_p2p as p2p;

use grin_util as util;
use grin_util::StopState;

use crate::core::core::hash::Hash;
use crate::core::global;
use crate::core::pow::Difficulty;
use crate::p2p::msg::built_info;
use crate::p2p::types::PeerAddr;
use ed25519_dalek::{Signer, SigningKey};
use grin_p2p::msg::PeerAddrs;
use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::{thread, time};

fn open_port() -> u16 {
	// use port 0 to allow the OS to assign an open port
	// TcpListener's Drop impl will unbind the port as soon as
	// listener goes out of scope
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	listener.local_addr().unwrap().port()
}

// Setup test with AutomatedTesting chain_type;
fn test_setup() {
	// Set "global" chain type here as we spawn peer threads for read/write.
	global::init_global_chain_type(global::ChainTypes::AutomatedTesting);
	util::init_test_logger();
}

fn clean_output_dir(dir_name: &str) {
	let _ = fs::remove_dir_all(dir_name);
}

fn mwixnet_announcement(key: &SigningKey, now: u64) -> p2p::mwixnet_protocol::RouteAnnouncement {
	use p2p::mwixnet_protocol::{
		Hash as MwixnetHash, MwixnetType, OnionAddress, PublicKey, RouteAnnouncement, RouteState,
		Signature, MWIXNET_PROTOCOL_VERSION,
	};
	let identity = PublicKey(key.verifying_key().to_bytes());
	let mut item = RouteAnnouncement {
		version: MWIXNET_PROTOCOL_VERSION,
		msg_type: MwixnetType::RouteAnnouncement,
		route_id: MwixnetHash([1; 32]),
		manifest_sequence: 1,
		entry_onion: OnionAddress(identity.0),
		swap_identity: identity,
		hop_count: 2,
		participant_identities: vec![
			identity,
			PublicKey(SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes()),
		],
		fee_per_hop: 1,
		manifest_hash: MwixnetHash([2; 32]),
		health_hash: MwixnetHash([4; 32]),
		status: RouteState::Healthy,
		last_verified: now,
		valid_until: now + 600,
		sequence: 1,
		signature: Signature([0; 64]),
	};
	item.signature = Signature(key.sign(item.hash().as_bytes()).to_bytes());
	item
}

fn mwixnet_offer(key: &SigningKey, now: u64) -> p2p::mwixnet_protocol::OfferAnnouncement {
	use p2p::mwixnet_protocol::{
		MixerOffer, MwixnetOffer, MwixnetType, OfferAnnouncement, OnionAddress, OnionPublicKey,
		PublicKey, Signature, MWIXNET_PROTOCOL_VERSION,
	};
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

fn p2p_server(
	dir: &str,
	peers_allow: Vec<PeerAddr>,
	peers_deny: Vec<PeerAddr>,
	port: Option<u16>,
) -> (SocketAddr, Arc<p2p::Server>) {
	p2p_server_with_adapter(
		dir,
		peers_allow,
		peers_deny,
		port,
		p2p::Capabilities::UNKNOWN,
		Arc::new(p2p::DummyAdapter::default()),
	)
}

fn p2p_server_with_adapter(
	dir: &str,
	peers_allow: Vec<PeerAddr>,
	peers_deny: Vec<PeerAddr>,
	port: Option<u16>,
	capabilities: p2p::Capabilities,
	net_adapter: Arc<dyn p2p::ChainAdapter>,
) -> (SocketAddr, Arc<p2p::Server>) {
	let p2p_config = p2p::P2PConfig {
		host: "127.0.0.1".parse().unwrap(),
		port: port.unwrap_or_else(|| open_port()),
		peers_allow: if peers_allow.is_empty() {
			None
		} else {
			Some(PeerAddrs { peers: peers_allow })
		},
		peers_deny: if peers_deny.is_empty() {
			None
		} else {
			Some(PeerAddrs { peers: peers_deny })
		},
		..p2p::P2PConfig::default()
	};
	let server = Arc::new(
		p2p::Server::new(
			dir,
			capabilities,
			p2p_config.clone(),
			net_adapter.clone(),
			Hash::from_vec(&vec![]),
			Arc::new(StopState::new()),
		)
		.unwrap(),
	);

	let p2p_inner = server.clone();
	let _ = thread::spawn(move || p2p_inner.listen());

	thread::sleep(time::Duration::from_secs(1));

	let addr = SocketAddr::new(p2p_config.host, p2p_config.port);
	(addr, server)
}

#[test]
fn peer_handshake() {
	test_setup();
	let test_dir = "target/peer_handshake";
	clean_output_dir(test_dir);

	// Start peers and connect to check handshake, checking ping/pong exchange.
	{
		let (_, server) = p2p_server(test_dir, vec![], vec![], None);
		let (peer_addr, _) = p2p_server(test_dir, vec![], vec![], None);

		let peer = server.connect(PeerAddr(peer_addr)).unwrap();

		let git_hash =
			built_info::GIT_COMMIT_HASH_SHORT.map_or_else(|| "".to_owned(), |v| ".".to_owned() + v);
		assert!(peer
			.info
			.user_agent
			.ends_with(format!("{}{}", env!("CARGO_PKG_VERSION"), git_hash).as_str()));

		thread::sleep(time::Duration::from_secs(1));

		peer.send_ping(Difficulty::min_dma(), 0).unwrap();
		thread::sleep(time::Duration::from_secs(1));

		let server_peer = server
			.peers
			.get_connected_peer(PeerAddr(peer_addr))
			.unwrap();
		assert_eq!(server_peer.info.total_difficulty(), Difficulty::min_dma());
		assert!(server.peers.iter().connected().count() > 0);
	}

	// Start a server allowing connections from/to peer at "allow" list.
	{
		let port = open_port();
		let allow_port = open_port();
		let other_port = open_port();
		let allow_addr = PeerAddr(format!("127.0.0.1:{}", allow_port).parse().unwrap());
		let (addr, server) = p2p_server(test_dir, vec![allow_addr], vec![], Some(port));

		let (addr2, server2) = p2p_server(test_dir, vec![], vec![], Some(allow_port));

		// Inbound connection test.
		let peer = server2.connect(PeerAddr(addr)).unwrap();
		peer.send_ping(Difficulty::min_dma(), 0).unwrap();
		thread::sleep(time::Duration::from_secs(1));

		assert!(server2.peers.iter().connected().count() > 0);

		server2
			.peers
			.disconnect_peer(PeerAddr(addr), "Inbound test finished")
			.unwrap();
		thread::sleep(time::Duration::from_secs(1));

		// Outbound connection test.
		let peer = server.connect(PeerAddr(addr2)).unwrap();
		peer.send_ping(Difficulty::min_dma(), 0).unwrap();
		thread::sleep(time::Duration::from_secs(1));

		let server_peer = server.peers.get_connected_peer(allow_addr).unwrap();
		assert_eq!(server_peer.info.total_difficulty(), Difficulty::min_dma());
		assert!(server.peers.iter().connected().count() > 0);

		server
			.peers
			.disconnect_peer(PeerAddr(addr2), "Outbound test finished")
			.unwrap();
		thread::sleep(time::Duration::from_secs(1));

		// Block connections from/to peer not from "allow" list.
		let (addr3, server3) = p2p_server(test_dir, vec![], vec![], Some(other_port));

		assert!(server.connect(PeerAddr(addr3)).is_err());
		assert!(server3.connect(PeerAddr(addr)).is_err());
		assert_eq!(server.peers.iter().connected().count(), 0);
	}

	// Start a server to refuse peer from "deny" list.
	{
		let port = open_port();
		let deny_port = open_port();
		let deny_addr = PeerAddr(format!("127.0.0.1:{}", deny_port).parse().unwrap());
		let (addr, server) = p2p_server(test_dir, vec![], vec![deny_addr], Some(port));

		let (addr2, server2) = p2p_server(test_dir, vec![], vec![], Some(deny_port));

		// Inbound connection test.
		assert!(server2.connect(PeerAddr(addr)).is_err());
		assert_eq!(server.peers.iter().connected().count(), 0);

		// Outbound connection test.
		assert!(server.connect(PeerAddr(addr2)).is_err());
		assert_eq!(server.peers.iter().connected().count(), 0);
	}
}

#[test]
fn mwixnet_relay_between_peers() {
	test_setup();
	let first_dir = "target/mwixnet_relay_first";
	let second_dir = "target/mwixnet_relay_second";
	clean_output_dir(first_dir);
	clean_output_dir(second_dir);

	let first_cache = Arc::new(p2p::RouteCache::new(true, vec![]));
	let second_cache = Arc::new(p2p::RouteCache::new(true, vec![]));
	let capabilities =
		p2p::Capabilities::MWIXNET_ROUTE_RELAY | p2p::Capabilities::MWIXNET_OFFER_RELAY;
	let (_, first) = p2p_server_with_adapter(
		first_dir,
		vec![],
		vec![],
		None,
		capabilities,
		Arc::new(p2p::DummyAdapter::with_mwixnet_routes(first_cache.clone())),
	);
	let (second_addr, second) = p2p_server_with_adapter(
		second_dir,
		vec![],
		vec![],
		None,
		capabilities,
		Arc::new(p2p::DummyAdapter::with_mwixnet_routes(second_cache.clone())),
	);
	first.connect(PeerAddr(second_addr)).unwrap();

	let now = chrono::Utc::now().timestamp() as u64;
	let key = SigningKey::from_bytes(&[7; 32]);
	let route =
		p2p::mwixnet_protocol::RouteRelayItem::Announcement(mwixnet_announcement(&key, now));
	let offer = mwixnet_offer(&key, now);
	first_cache.insert(route.clone(), None).unwrap();
	first_cache.insert_offer(offer.clone(), None).unwrap();
	first.peers.broadcast_mwixnet_route(&route, None);
	first.peers.broadcast_mwixnet_offer(&offer, None);

	for _ in 0..50 {
		let routes = second_cache.page(None, 1).unwrap().1;
		let offers = second_cache.offer_page(None, 1).unwrap().1;
		if routes == vec![route.clone()] && offers == vec![offer.clone()] {
			break;
		}
		thread::sleep(time::Duration::from_millis(100));
	}
	assert_eq!(second_cache.page(None, 1).unwrap().1, vec![route]);
	assert_eq!(second_cache.offer_page(None, 1).unwrap().1, vec![offer]);

	first.stop();
	second.stop();
	clean_output_dir(first_dir);
	clean_output_dir(second_dir);
}

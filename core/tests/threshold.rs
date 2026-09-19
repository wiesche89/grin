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

//! Exercise threshold key contributions through the ordinary kernel and output verifiers.
//! Dealer-generated polynomials are test fixtures, not a wallet initialization protocol.

use grin_core::core::{KernelFeatures, Output, OutputFeatures, TxKernel};
use grin_core::libtx::{aggsig, multisig::lagrange_coefficient, proof};
use rand::thread_rng;
use util::secp::key::{PublicKey, SecretKey, ZERO_KEY};
use util::secp::pedersen::{Commitment, ProofMessage, RangeProof};
use util::secp::Secp256k1;

fn scalar(secp: &Secp256k1, value: u64) -> SecretKey {
	let mut bytes = [0; 32];
	bytes[24..].copy_from_slice(&value.to_be_bytes());
	SecretKey::from_slice(secp, &bytes).unwrap()
}

// Degree threshold-1, with a nonzero leading coefficient.
fn polynomial(x: u64, threshold: usize) -> u64 {
	(0..threshold)
		.map(|power| (17 + power as u64 * 6) * x.pow(power as u32))
		.sum()
}

fn contributions(
	secp: &Secp256k1,
	selected: &[u64],
	threshold: usize,
	at: Option<&SecretKey>,
) -> Vec<SecretKey> {
	let coordinates: Vec<_> = selected.iter().map(|x| scalar(secp, *x)).collect();
	selected
		.iter()
		.zip(&coordinates)
		.map(|(x, coordinate)| {
			let coefficient = lagrange_coefficient(secp, coordinate, &coordinates, at).unwrap();
			let mut share = scalar(secp, polynomial(*x, threshold));
			let mut verification_key = PublicKey::from_secret_key(secp, &share).unwrap();
			verification_key.mul_assign(secp, &coefficient).unwrap();
			share.mul_assign(secp, &coefficient).unwrap();
			assert_eq!(
				PublicKey::from_secret_key(secp, &share).unwrap(),
				verification_key
			);
			share
		})
		.collect()
}

fn kernel(
	secp: &Secp256k1,
	shares: &[SecretKey],
	public_key: &PublicKey,
	adaptor: bool,
) -> TxKernel {
	let features = KernelFeatures::Plain { fee: 0.into() };
	let msg = features.kernel_sig_msg().unwrap();
	let nonces: Vec<_> = shares
		.iter()
		.map(|_| SecretKey::new(secp, &mut thread_rng()))
		.collect();
	let public_nonces: Vec<_> = nonces
		.iter()
		.map(|n| PublicKey::from_secret_key(secp, n).unwrap())
		.collect();
	let nonce_sum = PublicKey::from_combination(secp, public_nonces.iter().collect()).unwrap();
	let mut signatures = Vec::new();
	for (share, nonce) in shares.iter().zip(&nonces) {
		let sig =
			aggsig::calculate_partial_sig(secp, share, nonce, &nonce_sum, Some(public_key), &msg)
				.unwrap();
		let verification_key = PublicKey::from_secret_key(secp, share).unwrap();
		aggsig::verify_partial_sig(
			secp,
			&sig,
			&nonce_sum,
			&verification_key,
			Some(public_key),
			&msg,
		)
		.unwrap();
		signatures.push(sig);
	}
	let final_sig = aggsig::add_signatures(secp, signatures.iter().collect(), &nonce_sum).unwrap();
	if adaptor {
		let secret = SecretKey::new(secp, &mut thread_rng());
		let public = PublicKey::from_secret_key(secp, &secret).unwrap();
		let sig = aggsig::calculate_partial_sig_with_adaptor(
			secp,
			&shares[0],
			&nonces[0],
			&secret,
			&nonce_sum,
			Some(public_key),
			&msg,
		)
		.unwrap();
		let verification_key = PublicKey::from_secret_key(secp, &shares[0]).unwrap();
		aggsig::verify_partial_sig_with_adaptor(
			secp,
			&sig,
			&nonce_sum,
			&public,
			&verification_key,
			Some(public_key),
			&msg,
		)
		.unwrap();
		let mut recovered = SecretKey::from_slice(secp, &sig.as_ref()[32..]).unwrap();
		// Recover from the completed kernel and the other participants' signatures.
		for other in &signatures[1..] {
			recovered
				.add_assign(
					secp,
					&SecretKey::from_slice(secp, &other.as_ref()[32..]).unwrap(),
				)
				.unwrap();
		}
		let mut plain = SecretKey::from_slice(secp, &final_sig.as_ref()[32..]).unwrap();
		plain.neg_assign(secp).unwrap();
		recovered.add_assign(secp, &plain).unwrap();
		assert_eq!(recovered, secret);
	}
	TxKernel {
		features,
		excess: Commitment::from_pubkey(secp, public_key).unwrap(),
		excess_sig: final_sig,
	}
}

fn rangeproof(
	secp: &Secp256k1,
	shares: &[SecretKey],
	commit: Commitment,
	extra_data: Option<Vec<u8>>,
) -> Result<RangeProof, grin_core::libtx::Error> {
	let nonce = SecretKey::new(secp, &mut thread_rng());
	let private: Vec<_> = shares
		.iter()
		.map(|_| SecretKey::new(secp, &mut thread_rng()))
		.collect();
	let mut first = vec![PublicKey::new(); shares.len()];
	let mut second = first.clone();
	for i in 0..shares.len() {
		proof::create_multisig_with_key(
			secp,
			42,
			&shares[i],
			&nonce,
			&private[i],
			ProofMessage::empty(),
			None,
			Some(&mut first[i]),
			Some(&mut second[i]),
			&[commit],
			1,
			extra_data.clone(),
		)
		.unwrap();
	}
	let individual_first = first.clone();
	let individual_second = second.clone();
	let mut first = PublicKey::from_combination(secp, first.iter().collect()).unwrap();
	let mut second = PublicKey::from_combination(secp, second.iter().collect()).unwrap();
	let mut taus = Vec::new();
	for i in 0..shares.len() {
		let mut tau = ZERO_KEY;
		proof::create_multisig_with_key(
			secp,
			42,
			&shares[i],
			&nonce,
			&private[i],
			ProofMessage::empty(),
			Some(&mut tau),
			Some(&mut first),
			Some(&mut second),
			&[commit],
			2,
			extra_data.clone(),
		)
		.unwrap();
		taus.push(tau);
	}
	let mut tau = secp.blind_sum(taus.clone(), vec![]).unwrap();
	let proof = proof::create_multisig_with_key(
		secp,
		42,
		&shares[0],
		&nonce,
		&private[0],
		ProofMessage::empty(),
		Some(&mut tau),
		Some(&mut first),
		Some(&mut second),
		&[commit],
		0,
		extra_data.clone(),
	)?
	.unwrap();
	for i in 0..shares.len() {
		let public = PublicKey::from_secret_key(secp, &shares[i]).unwrap();
		let check = |candidate: &RangeProof,
		             tau: &SecretKey,
		             key: &PublicKey,
		             t1: &PublicKey,
		             t2: &PublicKey,
		             extra: Option<&[u8]>| {
			proof::verify_multisig_partial(secp, commit, candidate, extra, key, t1, t2, tau)
		};
		assert!(check(
			&proof,
			&taus[i],
			&public,
			&individual_first[i],
			&individual_second[i],
			extra_data.as_deref()
		)
		.is_ok());
		let wrong_commit = secp.commit(43, shares[i].clone()).unwrap();
		assert!(proof::verify_multisig_partial(
			secp,
			wrong_commit,
			&proof,
			extra_data.as_deref(),
			&public,
			&individual_first[i],
			&individual_second[i],
			&taus[i]
		)
		.is_err());
		let mut incorrect_tau = taus[i].clone();
		incorrect_tau.add_assign(secp, &scalar(secp, 1)).unwrap();
		assert!(check(
			&proof,
			&incorrect_tau,
			&public,
			&individual_first[i],
			&individual_second[i],
			extra_data.as_deref()
		)
		.is_err());
		assert!(check(
			&proof,
			&taus[i],
			&public,
			&individual_second[i],
			&individual_first[i],
			extra_data.as_deref()
		)
		.is_err());
		let wrong_key = PublicKey::from_secret_key(secp, &scalar(secp, 99)).unwrap();
		assert!(check(
			&proof,
			&taus[i],
			&wrong_key,
			&individual_first[i],
			&individual_second[i],
			extra_data.as_deref()
		)
		.is_err());
		assert!(check(
			&proof,
			&taus[i],
			&public,
			&individual_first[i],
			&individual_second[i],
			Some(b"wrong session")
		)
		.is_err());
		// A bad aggregate tau must not prevent identifying individual bad shares.
		let mut candidate = proof;
		candidate.proof[0] ^= 1;
		assert!(proof::verify(secp, commit, candidate, extra_data.clone()).is_err());
		assert!(check(
			&candidate,
			&taus[i],
			&public,
			&individual_first[i],
			&individual_second[i],
			extra_data.as_deref()
		)
		.is_ok());
		assert!(check(
			&candidate,
			&incorrect_tau,
			&public,
			&individual_first[i],
			&individual_second[i],
			extra_data.as_deref()
		)
		.is_err());
	}
	Ok(proof)
}

#[test]
fn threshold_kernels_and_outputs() {
	let secp = Secp256k1::with_caps(util::secp::ContextFlag::Commit);
	for (threshold, total) in [(1, 1), (1, 3), (2, 3), (3, 5)] {
		for at in [0, 7] {
			let coordinate = if at == 0 {
				None
			} else {
				Some(scalar(&secp, at))
			};
			// A public coin-key complement is added once, independently of the quorum.
			let tweak = if at == 0 { 0 } else { 11 };
			let expected = scalar(&secp, polynomial(at, threshold) + tweak);
			let public = PublicKey::from_secret_key(&secp, &expected).unwrap();
			let commit = secp.commit(42, expected).unwrap();
			for mask in 1u32..(1 << total) {
				let selected: Vec<_> = (0..total)
					.filter(|i| mask & (1 << i) != 0)
					.map(|i| i + 1)
					.collect();
				let mut shares = contributions(&secp, &selected, threshold, coordinate.as_ref());
				if tweak != 0 {
					shares[0].add_assign(&secp, &scalar(&secp, tweak)).unwrap();
				}
				let enough = selected.len() >= threshold;
				assert_eq!(
					kernel(&secp, &shares, &public, enough).verify().is_ok(),
					enough
				);
				let proof = rangeproof(&secp, &shares, commit, None);
				assert_eq!(proof.is_ok(), enough);
				if let Ok(proof) = proof {
					Output::new(OutputFeatures::Plain, commit, proof)
						.verify_proof()
						.unwrap();
				}
			}
		}
	}
}

#[test]
fn interpolation_validates_coordinates() {
	let secp = Secp256k1::new();
	let one = scalar(&secp, 1);
	let two = scalar(&secp, 2);
	let three = scalar(&secp, 3);
	let participants = [one.clone(), two.clone()];
	assert_eq!(
		lagrange_coefficient(&secp, &one, &participants, None).unwrap(),
		two
	);
	let mut minus_one = one.clone();
	minus_one.neg_assign(&secp).unwrap();
	assert_eq!(
		lagrange_coefficient(&secp, &two, &participants, None).unwrap(),
		minus_one
	);
	assert_eq!(
		lagrange_coefficient(&secp, &one, &[two.clone(), one.clone()], None).unwrap(),
		two
	);
	let mut half =
		lagrange_coefficient(&secp, &one, &[one.clone(), minus_one.clone()], None).unwrap();
	assert_eq!(
		half,
		lagrange_coefficient(&secp, &minus_one, &[one.clone(), minus_one.clone()], None).unwrap()
	);
	half.mul_assign(&secp, &two).unwrap();
	assert_eq!(half, one);
	assert!(lagrange_coefficient(&secp, &one, &[], None).is_err());
	assert!(lagrange_coefficient(&secp, &three, &participants, None).is_err());
	assert!(lagrange_coefficient(&secp, &one, &[one.clone(), one.clone()], None).is_err());
	assert!(lagrange_coefficient(&secp, &one, &[one.clone(), ZERO_KEY], None).is_err());
	assert!(lagrange_coefficient(&secp, &one, &[one.clone(), SecretKey([255; 32])], None).is_err());
	assert!(lagrange_coefficient(&secp, &one, &participants, Some(&one)).is_err());
	assert!(lagrange_coefficient(&secp, &one, &participants, Some(&ZERO_KEY)).is_err());
}

#[test]
fn partial_proofs_with_extra_data() {
	let secp = Secp256k1::with_caps(util::secp::ContextFlag::Commit);
	let shares = contributions(&secp, &[1, 3, 5], 3, None);
	let commit = secp.commit(42, scalar(&secp, polynomial(0, 3))).unwrap();
	for extra in [Some(vec![]), Some(b"proof session".to_vec())] {
		let proof = rangeproof(&secp, &shares, commit, extra.clone()).unwrap();
		assert!(proof::verify(&secp, commit, proof, extra).is_ok());
	}
}

#[test]
fn invalid_proof_rounds() {
	let secp = Secp256k1::with_caps(util::secp::ContextFlag::Commit);
	let key = scalar(&secp, 1);
	let commit = secp.commit(42, key.clone()).unwrap();
	for round in 0..=3 {
		let mut tau = ZERO_KEY;
		let mut t1 = PublicKey::new();
		let mut t2 = PublicKey::new();
		assert!(proof::create_multisig_with_key(
			&secp,
			42,
			&key,
			&key,
			&key,
			ProofMessage::empty(),
			None,
			None,
			None,
			&[commit],
			round,
			None
		)
		.is_err());
		assert!(proof::create_multisig_with_key(
			&secp,
			42,
			&key,
			&key,
			&key,
			ProofMessage::empty(),
			Some(&mut tau),
			Some(&mut t1),
			Some(&mut t2),
			&[commit],
			round,
			None
		)
		.is_err());
		assert!(!t1.is_valid());
		assert!(!t2.is_valid());
		assert_eq!(tau, ZERO_KEY);
	}
	let public = PublicKey::from_secret_key(&secp, &key).unwrap();
	for length in [0, 192, usize::MAX] {
		let mut proof = RangeProof::zero();
		proof.plen = length;
		assert!(proof::verify_multisig_partial(
			&secp, commit, &proof, None, &public, &public, &public, &key
		)
		.is_err());
	}
}

#[test]
fn invalid_proof_inputs_leave_outputs_unchanged() {
	let secp = Secp256k1::with_caps(util::secp::ContextFlag::Commit);
	let key = scalar(&secp, 1);
	let commit = secp.commit(42, key.clone()).unwrap();
	let public = PublicKey::from_secret_key(&secp, &key).unwrap();
	let mut t1 = public;
	let mut t2 = public;
	for commits in [vec![], vec![commit, commit], vec![Commitment([0; 33])]] {
		assert!(proof::create_multisig_with_key(
			&secp,
			42,
			&key,
			&key,
			&key,
			ProofMessage::empty(),
			None,
			Some(&mut t1),
			Some(&mut t2),
			&commits,
			1,
			None
		)
		.is_err());
	}
	for invalid in [ZERO_KEY, SecretKey([255; 32])] {
		assert!(proof::create_multisig_with_key(
			&secp,
			42,
			&invalid,
			&key,
			&key,
			ProofMessage::empty(),
			None,
			Some(&mut t1),
			Some(&mut t2),
			&[commit],
			1,
			None
		)
		.is_err());
	}
	assert!(proof::create_multisig_with_key(
		&Secp256k1::new(),
		42,
		&key,
		&key,
		&key,
		ProofMessage::empty(),
		None,
		Some(&mut t1),
		Some(&mut t2),
		&[commit],
		1,
		None
	)
	.is_err());
	assert_eq!(t1, public);
	assert_eq!(t2, public);
}

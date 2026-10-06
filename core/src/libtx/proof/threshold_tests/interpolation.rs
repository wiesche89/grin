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

//! Interpolation of Shamir shares for transaction construction.

use crate::libtx::Error;
use util::secp::key::{SecretKey, ONE_KEY};
use util::secp::Secp256k1;

/// Lagrange coefficient for a test share. `None` evaluates at zero.
/// Reject invalid, duplicate or overlapping coordinates before interpolation.
pub(super) fn lagrange_coefficient(
	secp: &Secp256k1,
	participant: &SecretKey,
	participants: &[SecretKey],
	at: Option<&SecretKey>,
) -> Result<SecretKey, Error> {
	if participants.is_empty() || !participants.contains(participant) {
		return Err(Error::Other("Participant is not in the signing set".into()));
	}
	if let Some(at) = at {
		SecretKey::from_slice(secp, &at.0)?;
	}
	for (index, coordinate) in participants.iter().enumerate() {
		SecretKey::from_slice(secp, &coordinate.0)?;
		if participants[..index].contains(coordinate) || at == Some(coordinate) {
			return Err(Error::Other(
				"Duplicate or overlapping share coordinates".into(),
			));
		}
	}

	let mut coefficient = ONE_KEY;
	for other in participants.iter().filter(|other| *other != participant) {
		let mut negative = other.clone();
		negative.neg_assign(secp)?;
		let mut numerator = negative.clone();
		if let Some(at) = at {
			numerator.add_assign(secp, at)?;
		}
		let mut denominator = participant.clone();
		denominator.add_assign(secp, &negative)?;
		denominator.inv_assign(secp)?;
		numerator.mul_assign(secp, &denominator)?;
		coefficient.mul_assign(secp, &numerator)?;
	}
	Ok(coefficient)
}

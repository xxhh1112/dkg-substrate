// Copyright 2022 Webb Technologies Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
use crate::{
	utils::{find_index, set_up_rounds},
	worker::DKGWorker,
	Client,
};
use dkg_primitives::{
	crypto::AuthorityId,
	rounds::LocalKey,
	serde_json,
	types::RoundId,
	utils::{
		decrypt_data, encrypt_data, StoredLocalKey, DKG_LOCAL_KEY_FILE, QUEUED_DKG_LOCAL_KEY_FILE,
	},
};
use dkg_runtime_primitives::{
	offchain::crypto::{Pair as AppPair, Public},
	DKGApi,
};
use log::debug;
use sc_client_api::Backend;
use sp_api::{BlockT as Block, HeaderT as Header};
use sp_core::Pair;
use std::{
	fs,
	io::{Error, ErrorKind},
	path::PathBuf,
};

use curv::elliptic::curves::Secp256k1;

pub struct DKGPersistenceState {
	pub initial_check: bool,
}

impl DKGPersistenceState {
	pub fn new() -> Self {
		Self { initial_check: false }
	}
}

pub(crate) fn store_localkey<B, C, BE>(
	key: LocalKey<Secp256k1>,
	round_id: RoundId,
	path: Option<PathBuf>,
	worker: &mut DKGWorker<B, C, BE>,
) -> std::io::Result<()>
where
	B: Block,
	BE: Backend<B>,
	C: Client<B, BE>,
	C::Api: DKGApi<B, AuthorityId, <<B as Block>::Header as Header>::Number>,
{
	if let Some(path) = path {
		if let Some(local_keystore) = worker.local_keystore.clone() {
			debug!(target: "dkg_persistence", "Storing local key for {:?}", &path);
			let key_pair = local_keystore.as_ref().key_pair::<AppPair>(
				&Public::try_from(&worker.get_sr25519_public_key().0[..])
					.unwrap_or_else(|_| panic!("Could not find keypair in local key store")),
			);

			if let Ok(Some(key_pair)) = key_pair {
				let secret_key = key_pair.to_raw_vec();

				let stored_local_key = StoredLocalKey { round_id, local_key: key };
				let serialized_data = serde_json::to_string(&stored_local_key)
					.map_err(|_| Error::new(ErrorKind::Other, "Serialization failed"))?;

				let encrypted_data = encrypt_data(serialized_data.into_bytes(), secret_key)
					.map_err(|e| Error::new(ErrorKind::Other, e))?;
				fs::write(path.clone(), &encrypted_data[..])?;

				debug!(target: "dkg_persistence", "Successfully stored local key for {:?}", &path);
				Ok(())
			} else {
				Err(Error::new(
					ErrorKind::Other,
					"Local key pair doesn't exist for sr25519 key".to_string(),
				))
			}
		} else {
			Err(Error::new(ErrorKind::Other, "Local keystore doesn't exist".to_string()))
		}
	} else {
		Err(Error::new(ErrorKind::Other, "Path not defined".to_string()))
	}
}

/// Loads a stored `StoredLocalKey` from the file system.
/// Expects the local keystore to exist in order to retrieve the file.
/// Expects there to be an sr25519 keypair with `KEY_TYPE` = `ACCOUNT`.
///
/// Uses the raw keypair as a seed for a secret key input to the XChaCha20Poly1305
/// encryption cipher.
pub(crate) fn load_stored_key<B, C, BE>(
	path: PathBuf,
	worker: &mut DKGWorker<B, C, BE>,
) -> std::io::Result<StoredLocalKey>
where
	B: Block,
	BE: Backend<B>,
	C: Client<B, BE>,
	C::Api: DKGApi<B, AuthorityId, <<B as Block>::Header as Header>::Number>,
{
	if let Some(local_keystore) = worker.local_keystore.clone() {
		debug!(target: "dkg_persistence", "Loading local key for {:?}", &path);
		let key_pair = local_keystore.as_ref().key_pair::<AppPair>(
			&Public::try_from(&worker.get_sr25519_public_key().0[..])
				.unwrap_or_else(|_| panic!("Could not find keypair in local key store")),
		);

		if let Ok(Some(key_pair)) = key_pair {
			let secret_key = key_pair.to_raw_vec();

			let encrypted_data = fs::read(path.clone())?;
			let decrypted_data = decrypt_data(encrypted_data, secret_key)
				.map_err(|e| Error::new(ErrorKind::Other, e))?;
			let stored_local_key: StoredLocalKey = serde_json::from_slice(&decrypted_data)
				.map_err(|_| Error::new(ErrorKind::Other, "Deserialization failed"))?;
			Ok(stored_local_key)
		} else {
			Err(Error::new(
				ErrorKind::Other,
				"Local key pair doesn't exist for sr25519 key".to_string(),
			))
		}
	} else {
		Err(Error::new(ErrorKind::Other, "Local keystore doesn't exist".to_string()))
	}
}

/// We only try to resume the dkg once, if we can find any data for the completed offline stage for
/// the current round
pub(crate) fn try_resume_dkg<B, C, BE>(worker: &mut DKGWorker<B, C, BE>, header: &B::Header)
where
	B: Block,
	BE: Backend<B>,
	C: Client<B, BE>,
	C::Api: DKGApi<B, AuthorityId, <<B as Block>::Header as Header>::Number>,
{
	// We only try to resume the dkg once even if there is no data to recover
	if worker.dkg_persistence.initial_check {
		return
	}
	// Set initial check to prevent re-running resuming the dkg
	worker.dkg_persistence.initial_check = true;

	// If the rounds are already set, we return
	if worker.rounds.is_some() || worker.next_rounds.is_some() {
		return
	}

	// If there is no base path or local keystore then there is no DKG to resume.
	// We return in this case.
	if worker.local_keystore.is_none() || worker.base_path.is_none() {
		return
	}

	debug!(target: "dkg_persistence", "Trying to restore key gen data");
	if let Some((active, queued)) = worker.validator_set(header) {
		worker.current_validator_set = active.clone();
		worker.queued_validator_set = queued.clone();

		let public = worker.get_authority_public_key();
		// Set local key paths
		let base_path = worker.base_path.as_ref().unwrap();
		let local_key_path = base_path.join(DKG_LOCAL_KEY_FILE);
		let queued_local_key_path = base_path.join(QUEUED_DKG_LOCAL_KEY_FILE);
		// Set round IDs
		let round_id = active.id;
		let queued_round_id = queued.id;
		// Get the stored keys and check whether their rounds match any of the authority set IDs
		let mut local_key = load_stored_key(local_key_path.clone(), worker).ok();
		let mut queued_local_key = load_stored_key(queued_local_key_path.clone(), worker).ok();
		// Check if active key is outdated
		if let Some(active_key) = local_key.clone() {
			if active_key.round_id < round_id {
				local_key = None;
			}
		}
		// Swap the queued local key with the active local key if it matches active round ID
		if let Some(queued_key) = queued_local_key.clone() {
			if queued_key.round_id == round_id {
				local_key = queued_local_key;
				queued_local_key = None;
			}
			if queued_key.round_id < round_id {
				local_key = None;
				queued_local_key = None;
			}
		}
		// Get the best active authorities for setting up rounds
		let best_active_authorities: Vec<AuthorityId> =
			worker.get_best_authority_keys(header, active.clone(), true);
		// Create the active rounds only if the authority is selected in the best set
		if find_index::<AuthorityId>(&best_active_authorities[..], &public).is_some() {
			let jailed_signers = worker.get_signing_jailed(header, &best_active_authorities);
			let mut rounds = set_up_rounds(
				&best_active_authorities,
				round_id,
				&public,
				worker.get_signature_threshold(header),
				worker.get_keygen_threshold(header),
				Some(local_key_path),
				&jailed_signers,
			);

			if let Some(key) = local_key {
				debug!(target: "dkg_persistence", "Local key set");
				// Set the local key
				rounds.set_local_key(key.local_key);
				// Once local key is set, we can set the jailed signers which also
				// generates the next signing set.
				rounds.set_jailed_signers(jailed_signers);
				worker.rounds = Some(rounds);
			}
		}

		// Get the best queued authorities for setting up next rounds
		let best_queued_authorities: Vec<AuthorityId> =
			worker.get_best_authority_keys(header, queued.clone(), false);
		// Create next rounds only if the authority is selected in the best next set
		if find_index::<AuthorityId>(&best_queued_authorities[..], &public).is_some() {
			let jailed_signers = worker.get_signing_jailed(header, &best_queued_authorities);
			let mut rounds = set_up_rounds(
				&best_queued_authorities,
				queued_round_id,
				&public,
				worker.get_next_signature_threshold(header),
				worker.get_next_keygen_threshold(header),
				Some(queued_local_key_path.clone()),
				&jailed_signers,
			);

			if let Some(key) = queued_local_key {
				debug!(target: "dkg_persistence", "Queued local key set");
				// Set the local key
				rounds.set_local_key(key.local_key);
				// Once local key is set, we can set the jailed signers which also
				// generates the next signing set.
				rounds.set_jailed_signers(jailed_signers);
				worker.next_rounds = Some(rounds);
			}
		}
	}
}

/// To determine if the protocol should be restarted, we check if the
/// protocol is stuck at the keygen stage
#[allow(dead_code)]
pub(crate) fn should_restart_dkg<B, C, BE>(worker: &mut DKGWorker<B, C, BE>) -> (bool, bool)
where
	B: Block,
	BE: Backend<B>,
	C: Client<B, BE>,
	C::Api: DKGApi<B, AuthorityId, <<B as Block>::Header as Header>::Number>,
{
	let rounds = worker.rounds.take();
	let next_rounds = worker.next_rounds.take();

	let should_restart_rounds = {
		if let Some(rounds) = rounds {
			let stalled = rounds.has_stalled();
			worker.rounds = Some(rounds);
			stalled
		} else {
			false
		}
	};

	let should_restart_next_rounds = {
		if let Some(next_round) = next_rounds {
			let stalled = next_round.has_stalled();
			worker.next_rounds = Some(next_round);
			stalled
		} else {
			false
		}
	};

	(should_restart_rounds, should_restart_next_rounds)
}

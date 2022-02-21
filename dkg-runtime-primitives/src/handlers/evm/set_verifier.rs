use crate::{
	handlers::{decode_proposals::decode_proposal_header, validate_proposals::ValidationError},
	ChainIdTrait, ChainIdType, DKGPayloadKey, ProposalHeader,
};
use codec::alloc::string::ToString;
use ethereum_types::Address;

pub struct VerifierProposal<C: ChainIdTrait> {
	pub header: ProposalHeader<C>,
	pub new_verifier_address: Address,
}

/// https://github.com/webb-tools/protocol-solidity/issues/83
/// Proposal Data: [
///     resourceId          - 32 bytes [0..32]
///     functionSig         - 4 bytes  [32..36]
///     nonce               - 4 bytes  [36..40]
///     newVerifier         - 20 bytes [40..60]
/// ]
/// Total Bytes: 32 + 4 + 4 + 20 = 60
pub fn create<C: ChainIdTrait>(data: &[u8]) -> Result<VerifierProposal<C>, ValidationError> {
	if data.len() != 60 {
		return Err(ValidationError::InvalidParameter("Proposal data must be 60 bytes".to_string()))?
	}
	let header: ProposalHeader<C> = decode_proposal_header(data)?;

	let mut new_verifier_bytes = [0u8; 20];
	new_verifier_bytes.copy_from_slice(&data[40..60]);
	let new_verifier_address = Address::from(new_verifier_bytes);
	// TODO: Add validation over EVM address
	Ok(VerifierProposal { header, new_verifier_address })
}

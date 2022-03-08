import 'jest-extended';
import {BLOCK_TIME} from '../src/constants';
import {
	ethAddressFromUncompressedPublicKey,
	fetchDkgPublicKey,
	fetchDkgPublicKeySignature,
	fetchDkgRefreshNonce,
	sleep,
	triggerDkgManuaIncrementNonce,
	triggerDkgManualRefresh,
	waitForPublicKeySignatureToChange,
	waitForPublicKeyToChange,
} from '../src/utils';

import {
	localChain,
	polkadotApi,
	signatureBridge,
	executeAfter
} from './utils/util';

describe('Update SignatureBridge Governor', () => {
	test('should be able to transfer ownership to new Governor with Signature', async () => {
		// we trigger a manual renonce since we already transfered the ownership before.
		await triggerDkgManuaIncrementNonce(polkadotApi);
		// for some reason, we have to wait for a bit ¯\_(ツ)_/¯.
		await sleep(2 * BLOCK_TIME);
		// we trigger a manual DKG Refresh.
		await triggerDkgManualRefresh(polkadotApi);
		// then we wait until the dkg public key and its signature to get changed.
		await Promise.all([
			waitForPublicKeyToChange(polkadotApi),
			waitForPublicKeySignatureToChange(polkadotApi),
		]);
		// then we fetch them.
		const dkgPublicKey = await fetchDkgPublicKey(polkadotApi);
		const dkgPublicKeySignature = await fetchDkgPublicKeySignature(polkadotApi);
		const refreshNonce = await fetchDkgRefreshNonce(polkadotApi);
		expect(dkgPublicKey).toBeString();
		expect(dkgPublicKeySignature).toBeString();
		expect(refreshNonce).toBeGreaterThan(0);
		// now we can transfer ownership.
		const signatureSide = signatureBridge.getBridgeSide(localChain.chainId);
		const contract = signatureSide.contract;
		contract.connect(localChain.provider());
		const governor = await contract.governor();
		let nextGovernorAddress = ethAddressFromUncompressedPublicKey(dkgPublicKey!);
		// sanity check
		expect(nextGovernorAddress).not.toEqualCaseInsensitive(governor);
		const tx = await contract.transferOwnershipWithSignaturePubKey(
			dkgPublicKey!,
			refreshNonce,
			dkgPublicKeySignature!
		);
		await expect(tx.wait()).toResolve();
		// check that the new governor is the same as the one we just set.
		const newGovernor = await contract.governor();
		expect(newGovernor).not.toEqualCaseInsensitive(governor);
		expect(newGovernor).toEqualCaseInsensitive(nextGovernorAddress);
	});

	afterAll(async () => {
		await executeAfter();
	});
});
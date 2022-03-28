//! let (tx, rx) = futures::channel() // incoming
//! let (tx1, rx2) = futures::channel() // outgoing
//!
//! let state_machine = DKGStateMachine::new(...);
//! send_to_proper_locations_in_code(tx::<SignedDKGMessage>, rx2::<SignedDKGMessage>)
//! comment: sending: gossip_engine.lock().send
//! AsyncProtocol::new(
//!     state_machine,
//!     IncomingAsyncProtocolWrapper { receiver: rx },
//!     OutgoingAsyncProtocolWrapper { sender: tx1 },
//! ).run().await


use std::{task::{Context, Poll}, pin::Pin};

use codec::Encode;
use dkg_primitives::{types::{DKGError, DKGMessage, SignedDKGMessage, DKGMsgPayload}, crypto::Public, AuthoritySet};
use dkg_runtime_primitives::utils::to_slice_32;
use futures::{stream::Stream, Sink, TryStreamExt};
use log::error;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::state_machine::{keygen::Keygen};
use round_based::{Msg, async_runtime::AsyncProtocol, StateMachine, IsCritical};
use sp_core::sr25519;
use sp_runtime::{traits::Block, AccountId32};
use sc_client_api::Backend;
use crate::Client;

use crate::worker::DKGWorker;

use self::state_machine::DKGStateMachine;

pub struct IncomingAsyncProtocolWrapper<T> {
    pub receiver: futures::channel::mpsc::UnboundedReceiver<T>,
}

pub struct OutgoingAsyncProtocolWrapper<T: TransformOutgoing> {
    pub sender: futures::channel::mpsc::UnboundedSender<T::Output>,
}

pub trait TransformIncoming {
    type IncomingMapped;
    fn transform(self, party_index: u16, active: &[AccountId32], next: &[AccountId32]) -> Result<Msg<Self::IncomingMapped>, DKGError> where Self: Sized;
}

impl TransformIncoming for SignedDKGMessage<Public> {
    type IncomingMapped = DKGMessage<Public>;
    fn transform(self, party_index: u16, active: &[AccountId32], next: &[AccountId32]) -> Result<Msg<Self::IncomingMapped>, DKGError> where Self: Sized {
        verify_signature_against_authorities(self, active, next).map(|body| {
            Msg { sender: party_index, receiver: None, body }
        })
    }
}

/// Check a `signature` of some `data` against a `set` of accounts.
/// Returns true if the signature was produced from an account in the set and false otherwise.
fn check_signers(data: &[u8], signature: &[u8], set: &[AccountId32]) -> Result<bool, DKGError> {
	let maybe_signers = set.iter()
		.map(|x| {
			let val = x.encode().map_err(|err| DKGError::GenericError { reason: err.to_string() })?;
			let slice = to_slice_32(&val).ok_or_else(|| DKGError::GenericError { reason: "Failed to convert account id to sr25519 public key".to_string() })?;
			Ok(sr25519::Public(slice))
		}).collect::<Vec<Result<sr25519::Public, DKGError>>>();

	let mut processed = vec![];

	for maybe_signer in maybe_signers {
		processed.push(maybe_signer?);
	}

    let is_okay = dkg_runtime_primitives::utils::verify_signer_from_set(
        processed,
		collect(),
        data,
        signature,
    ).1;

	Ok(is_okay)
}

/// Verifies a SignedDKGMessage was signed by the active or next authorities
fn verify_signature_against_authorities(
    signed_dkg_msg: SignedDKGMessage<Public>,
    active_authorities: &[AccountId32],
    next_authorities: &[AccountId32],
) -> Result<DKGMessage<Public>, DKGError> {
    let dkg_msg = signed_dkg_msg.msg;
    let encoded = dkg_msg.encode();
    let signature = signed_dkg_msg.signature.unwrap_or_default();

    if check_signers(&encoded, &signature, active_authorities)? == true || check_signers(&encoded, &signature, next_authorities) == Ok(true) {
        Ok(dkg_msg)
    } else {
        Err(DKGError::GenericError {
            reason: "Message signature is not from a registered authority or next authority"
                .into(),
        })
    }
}


pub trait TransformOutgoing {
    type Output;
    fn transform(self) -> Result<Self::Output, DKGError> where Self: Sized;
}

impl<T> Stream for IncomingAsyncProtocolWrapper<T>
where
    T: TransformIncoming,
{
    type Item = Result<Msg<T>, DKGError>;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>
    ) -> Poll<Option<Self::Item>> {
        match futures::ready!(Pin::new(&mut self.receiver).poll_next(cx)) {
            Some(msg) => {
                match msg.transform() {
                    Ok(msg) => Poll::Ready(Some(Ok(msg))),
                    Err(e) => Poll::Ready(Some(Err(e))),
                }
            },
            None => Poll::Ready(None),
        }
    }
}

impl<T> Sink<Msg<T>> for OutgoingAsyncProtocolWrapper<T>
where
    T: TransformOutgoing
{
    type Error = DKGError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_ready(cx)
            .map_err(|err| DKGError::GenericError { reason: err.to_string() })
    }

    fn start_send(mut self: Pin<&mut Self>, item: Msg<T>) -> Result<(), Self::Error> {
        let transformed_item = item.body.transform(); // result<T::Output>
        match transformed_item {
            Ok(item) => Pin::new(&mut self.sender).start_send(item).map_err(|err| DKGError::GenericError { reason: err.to_string() }),
            Err(err) => {
                // log the error
                error!(target: "dkg", "🕸️  Failed to transform outgoing message: {:?}", err);
                Ok(())
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_flush(cx)
            .map_err(|err| DKGError::GenericError { reason: err.to_string() })
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.sender).poll_close(cx)
            .map_err(|err| DKGError::GenericError { reason: err.to_string() })
    }
}

pub trait MappedIncomingHandler {
    fn handle(self, message_queue: &mut Vec<Msg<Self>>) -> Result<(), DKGError>;
}

impl MappedIncomingHandler for DKGMessage<Public> {
    fn handle(self, message_queue: &mut Vec<Msg<Self>>) -> Result<(), DKGError> {
        let DKGMessage { id, payload, round_id } = self;

        match payload {
            DKGMsgPayload::Keygen(msg) => {
                match serde_json::from_slice::<Msg<ProtocolMessage>>(&msg.keygen_msg) {
                    Ok(msg) => {

                    }

                    Err(err) => {
                        return Err(DKGError::GenericError { reason: format!("Error deserializing keygen msg, reason: {}", err)})
                    }
                }
            }

            DKGMsgPayload::Offline(msg) => todo!(),

            DKGMsgPayload::Vote(msg) => todo!(),
            err => Err(DKGError::GenericError { reason: format!("{:?} ", err) })
        }
    }
}

pub mod meta_channel {
	use std::future::Future;
	use std::pin::Pin;
	use std::sync::Arc;
	use std::task::{Context, Poll};
	use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
	use futures::lock::Mutex;
	use futures::{select, StreamExt};
	use round_based::async_runtime::watcher::StderrWatcher;
	use round_based::AsyncProtocol;
	use sc_network_gossip::GossipEngine;
	use sp_runtime::traits::Block;
	use dkg_primitives::Public;
	use dkg_primitives::types::{DKGError, DKGKeygenMessage, DKGMessage, DKGOfflineMessage, DKGVoteMessage, SignedDKGMessage};
	use crate::async_protocol_handlers::state_machines::{KeygenStateMachine, OfflineStateMachine, VotingStateMachine};
	use crate::DKGKeystore;
	use crate::messages::dkg_message::sign_and_send_messages;

	pub type AsyncProtoType<SM, T> = AsyncProtocol<SM, UnboundedReceiver<T>, UnboundedSender<DKGMessage<Public>>, StderrWatcher>;
	pub trait SendFuture: Future<Output=Result<(), DKGError>> + Send {}
	impl<T> SendFuture for T where T: Future<Output=Result<(), DKGError>> + Send {}

	/// Once created, the MetaDKGMessageHandler should be .awaited to begin execution
	pub struct MetaDKGMessageHandler {
		to_keygen_state_machine: UnboundedSender<DKGKeygenMessage>,
		to_offline_state_machine: UnboundedSender<DKGOfflineMessage>,
		to_vote_state_machine: UnboundedSender<DKGVoteMessage>,
		protocol: Pin<Box<dyn SendFuture>>
	}

	impl MetaDKGMessageHandler {
		/// `to_outbound_wire` must be a function that takes DKGMessages and sends them outbound to
		/// the internet
		pub fn new<B>(gossip_engine: Arc<Mutex<GossipEngine<B>>>, keystore: DKGKeystore) -> Self
				where
		          B: Block {
			let (incoming_tx_keygen, incoming_rx_keygen) = futures::channel::mpsc::unbounded();
			let (outgoing_tx_keygen, outgoing_rx_keygen) = futures::channel::mpsc::unbounded();

			let (incoming_tx_offline, incoming_rx_offline) = futures::channel::mpsc::unbounded();
			let (outgoing_tx_offline, outgoing_rx_offline) = futures::channel::mpsc::unbounded();

			let (incoming_tx_vote, incoming_rx_vote) = futures::channel::mpsc::unbounded();
			let (outgoing_tx_vote, outgoing_rx_vote) = futures::channel::mpsc::unbounded();

			let keygen_state_machine = KeygenStateMachine { };
			let offline_state_machine = OfflineStateMachine { };
			let voting_state_machine = VotingStateMachine { };

			let ref mut keygen_proto = AsyncProtocol::new(keygen_state_machine, incoming_rx_keygen, outgoing_tx_keygen).set_watcher(StderrWatcher);
			let ref mut offline_proto = AsyncProtocol::new(offline_state_machine, incoming_rx_offline, outgoing_tx_offline).set_watcher(StderrWatcher);
			let ref mut voting_proto = AsyncProtocol::new(voting_state_machine, incoming_rx_vote, outgoing_tx_vote).set_watcher(StderrWatcher);

			let ref mut outgoing_to_wire = Box::pin(async move {
				let mut merged_stream = futures::stream_select!(outgoing_rx_keygen, outgoing_rx_offline, outgoing_rx_vote);

				while let Some(unsigned_message) = merged_stream.next().await {
					let unsigned_message: DKGMessage<Public> = unsigned_message;
					sign_and_send_messages(&gossip_engine,&keystore, vec![unsigned_message]).await;
				}

				Err(DKGError::CriticalError { reason: "Outbound stream stopped producing items".to_string() })
			});

			let protocol = async move {
				select! {
					keygen_res = keygen_proto.run() => {
						error!(target: "dkg", "🕸️  Keygen Protocol Ended: {:?}", keygen_res);
					},

					offline_res = offline_proto.run() => {
						error!(target: "dkg", "🕸️ Offline Protocol Ended: {:?}", offline_res);
					},

					voting_res = voting_proto.run() => {
						error!(target: "dkg", "🕸️  Voting Protocol Ended: {:?}", keygen_res);
					},

					outgoing = outgoing_to_wire => {
						error!(target: "dkg", "🕸️  Outbound Sender Ended: {:?}", keygen_res);
					}
				}

				// For now, return ok. Errors cna be propagated after further debugging
				Ok(())
			};


			Self {
				to_keygen_state_machine: incoming_tx_keygen,
				to_offline_state_machine: incoming_tx_offline,
				to_vote_state_machine: incoming_tx_vote,
				protocol: Box::pin(protocol)
			}
		}
	}

	impl Future for MetaDKGMessageHandler {
		type Output = Result<(), DKGError>;

		fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
			self.protocol.as_mut().poll(cx)
		}
	}
}

pub mod state_machines {
	use futures::channel::mpsc::UnboundedReceiver;
	use dkg_primitives::types::DKGKeygenMessage;

	pub struct KeygenStateMachine {

	}

	pub struct OfflineStateMachine {

	}

	pub struct VotingStateMachine;
}

pub mod state_machine {
    use std::marker::PhantomData;

    use dkg_primitives::types::DKGError;
    use round_based::{StateMachine, IsCritical, Msg};

    use super::{TransformOutgoing, MappedIncomingHandler};

    pub struct DKGStateMachine<T> {
        message_queue: Vec<Msg<T>>
    }

    impl IsCritical for DKGError {
        /// Indicates whether an error critical or not
        fn is_critical(&self) -> bool {
            match self {
                DKGError::CriticalError { .. } => true,
                _ => false
            }
        }
    }

    impl<T: MappedIncomingHandler> StateMachine for DKGStateMachine<T> {
        type MessageBody = T;
        type Err = DKGError;
        type Output = ();

        /// Process received message
        ///
        /// ## Returns
        /// Handling message might result in error, but it doesn't mean that computation should
        /// be aborted. Returned error needs to be examined whether it critical or not (by calling
        /// [is_critical](IsCritical::is_critical) method).
        ///
        /// If occurs:
        /// * Critical error: protocol must be aborted
        /// * Non-critical error: it should be reported, but protocol must continue
        ///
        /// Example of non-critical error is receiving message which we didn't expect to see. It could
        /// be either network lag or bug in implementation or attempt to sabotage the protocol, but
        /// protocol might be resistant to this, so it still has a chance to successfully complete.
        ///
        /// ## Blocking
        /// This method should not block or perform expensive computation. E.g. it might do
        /// deserialization (if needed) or cheap checks.
        fn handle_incoming(&mut self, msg: Msg<Self::MessageBody>) -> Result<(), Self::Err> {
            msg.body.handle(&mut self.message_queue)
        }

        /// Queue of messages to be sent
        ///
        /// New messages can be appended to queue only as result of calling
        /// [proceed](StateMachine::proceed) or [handle_incoming](StateMachine::handle_incoming) methods.
        ///
        /// Messages can be sent in any order. After message is sent, it should be deleted from the queue.
        fn message_queue(&mut self) -> &mut Vec<Msg<Self::MessageBody>> {
            &mut self.message_queue
        }

        /// Indicates whether StateMachine wants to perform some expensive computation
        fn wants_to_proceed(&self) -> bool;

        /// Performs some expensive computation
        ///
        /// If [`StateMachine`] is executed at green thread (in async environment), it will be typically
        /// moved to dedicated thread at thread pool before calling `.proceed()` method.
        ///
        /// ## Returns
        /// Returns `Ok(())` if either computation successfully completes or computation was not
        /// required (i.e. `self.wants_to_proceed() == false`).
        ///
        /// If it returns `Err(err)`, then `err` is examined whether it's critical or not (by
        /// calling [is_critical](IsCritical::is_critical) method).
        ///
        /// If occurs:
        /// * Critical error: protocol must be aborted
        /// * Non-critical error: it should be reported, but protocol must continue
        ///
        /// For example, in `.proceed()` at verification stage we could find some party trying to
        /// sabotage the protocol, but protocol might be designed to be resistant to such attack, so
        /// it's not a critical error, but obviously it should be reported.
        fn proceed(&mut self) -> Result<(), Self::Err> {

		}

        /// Deadline for a particular round
        ///
        /// After reaching deadline (if set) [round_timeout_reached](Self::round_timeout_reached)
        /// will be called.
        ///
        /// After proceeding on the next round (increasing [current_round](Self::current_round)),
        /// timer will be reset, new timeout will be requested (by calling this method), and new
        /// deadline will be set.
        fn round_timeout(&self) -> Option<Duration>;

        /// Method is triggered after reaching [round_timeout](Self::round_timeout)
        ///
        /// Reaching timeout always aborts computation, no matter what error is returned: critical or not.
        fn round_timeout_reached(&mut self) -> Self::Err;

        /// Indicates whether protocol is finished and output can be obtained by calling
        /// [pick_output](Self::pick_output) method.
        fn is_finished(&self) -> bool;

        /// Obtains protocol output
        ///
        /// ## Returns
        /// * `None`, if protocol is not finished yet
        ///   i.e. `protocol.is_finished() == false`
        /// * `Some(Err(_))`, if protocol terminated with error
        /// * `Some(Ok(_))`, if protocol successfully terminated
        ///
        /// After `Some(_)` has been obtained via this method, StateMachine must be utilized (dropped).
        fn pick_output(&mut self) -> Option<Result<Self::Output, Self::Err>>;

        /// Sequential number of current round
        ///
        /// Can be increased by 1 as result of calling either [proceed](StateMachine::proceed) or
        /// [handle_incoming](StateMachine::handle_incoming) methods. Changing round number in any other way
        /// (or in any other method) might cause strange behaviour.
        fn current_round(&self) -> u16;

        /// Total amount of rounds (if known)
        fn total_rounds(&self) -> Option<u16>;

        /// Index of this party
        ///
        /// Must be in interval `[1; n]` where `n = self.parties()`
        fn party_ind(&self) -> u16;
        /// Number of parties involved in computation
        fn parties(&self) -> u16;
    }
}
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::state_machine::traits::RoundBlame;
use round_based::{Msg, StateMachine};
use std::sync::Arc;

use super::meta_handler::CurrentRoundBlame;

pub(crate) struct StateMachineWrapper<T: StateMachine> {
	sm: T,
	current_round_blame: Arc<tokio::sync::watch::Sender<CurrentRoundBlame>>,
}

impl<T: StateMachine + RoundBlame> StateMachineWrapper<T> {
	pub fn new(
		sm: T,
		current_round_blame: Arc<tokio::sync::watch::Sender<CurrentRoundBlame>>,
	) -> Self {
		Self { sm, current_round_blame }
	}

	fn collect_round_blame(&self) {
		let (unrecieved_messages, blamed_parties) = self.round_blame();
		let _ = self
			.current_round_blame
			.send(CurrentRoundBlame { unrecieved_messages, blamed_parties });
	}
}

impl<T> StateMachine for StateMachineWrapper<T>
where
	T: StateMachine + RoundBlame,
{
	type Err = T::Err;
	type Output = T::Output;
	type MessageBody = T::MessageBody;

	fn handle_incoming(&mut self, msg: Msg<Self::MessageBody>) -> Result<(), Self::Err> {
		let result = self.sm.handle_incoming(msg);
		self.collect_round_blame();
		result
	}

	fn message_queue(&mut self) -> &mut Vec<Msg<Self::MessageBody>> {
		self.sm.message_queue()
	}

	fn wants_to_proceed(&self) -> bool {
		self.sm.wants_to_proceed()
	}

	fn proceed(&mut self) -> Result<(), Self::Err> {
		let result = self.sm.proceed();
		self.collect_round_blame();
		result
	}

	fn round_timeout(&self) -> Option<std::time::Duration> {
		self.sm.round_timeout()
	}

	fn round_timeout_reached(&mut self) -> Self::Err {
		let result = self.sm.round_timeout_reached();
		result
	}

	fn is_finished(&self) -> bool {
		self.sm.is_finished()
	}

	fn pick_output(&mut self) -> Option<Result<Self::Output, Self::Err>> {
		self.sm.pick_output()
	}

	fn current_round(&self) -> u16 {
		self.sm.current_round()
	}

	fn total_rounds(&self) -> Option<u16> {
		self.sm.total_rounds()
	}

	fn party_ind(&self) -> u16 {
		self.sm.party_ind()
	}

	fn parties(&self) -> u16 {
		self.sm.parties()
	}
}

impl<T: StateMachine + RoundBlame> RoundBlame for StateMachineWrapper<T> {
	fn round_blame(&self) -> (u16, Vec<u16>) {
		self.sm.round_blame()
	}
}

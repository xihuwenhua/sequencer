pub mod behaviour;
pub mod handler;
mod messages;
pub mod protocol;

#[cfg(test)]
mod flow_test;

use std::time::Duration;

pub use behaviour::{Behaviour, ToOtherBehaviourEvent};
use libp2p::{PeerId, StreamProtocol};

use crate::Bytes;

#[derive(Clone, Copy, Debug, Default, derive_more::Display, Eq, Hash, PartialEq)]
pub struct OutboundSessionId {
    pub value: usize,
}

#[derive(
    Clone, Copy, Debug, Default, derive_more::Display, Eq, Hash, PartialEq, PartialOrd, Ord,
)]
pub struct InboundSessionId {
    pub value: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionId {
    OutboundSessionId(OutboundSessionId),
    InboundSessionId(InboundSessionId),
}

impl From<OutboundSessionId> for SessionId {
    fn from(outbound_session_id: OutboundSessionId) -> Self {
        Self::OutboundSessionId(outbound_session_id)
    }
}

impl From<InboundSessionId> for SessionId {
    fn from(inbound_session_id: InboundSessionId) -> Self {
        Self::InboundSessionId(inbound_session_id)
    }
}

#[derive(Debug)]
pub enum GenericEvent<SessionError> {
    NewInboundSession {
        query: Bytes,
        inbound_session_id: InboundSessionId,
        peer_id: PeerId,
        protocol_name: StreamProtocol,
    },
    ReceivedResponse {
        outbound_session_id: OutboundSessionId,
        response: Bytes,
        peer_id: PeerId,
    },
    SessionFailed {
        session_id: SessionId,
        error: SessionError,
    },
    SessionFinishedSuccessfully {
        session_id: SessionId,
    },
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Config {
    pub session_timeout: Duration,
}

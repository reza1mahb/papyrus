use std::collections::HashMap;

use chrono::Duration;
use libp2p::swarm::dial_opts::DialOpts;
use libp2p::swarm::ToSwarm;
use libp2p::PeerId;

use self::behaviour_impl::Event;
use self::peer::PeerTrait;
use crate::db_executor::QueryId;
use crate::main_behaviour::mixed_behaviour;
use crate::streamed_bytes;

pub(crate) mod behaviour_impl;
pub(crate) mod peer;
#[cfg(test)]
mod test;

#[cfg_attr(test, derive(Debug, PartialEq))]
#[allow(dead_code)]
pub enum ReputationModifier {
    // TODO: Implement this enum
    Bad,
}

pub struct PeerManager<P: PeerTrait + 'static> {
    peers: HashMap<PeerId, P>,
    // TODO: consider implementing a cleanup mechanism to not store all queries forever
    query_to_peer_map: HashMap<QueryId, PeerId>,
    config: PeerManagerConfig,
    last_peer_index: usize,
    pending_events: Vec<ToSwarm<Event, libp2p::swarm::THandlerInEvent<Self>>>,
    peer_pending_dial_with_events:
        HashMap<PeerId, Vec<ToSwarm<Event, libp2p::swarm::THandlerInEvent<Self>>>>,
}

#[derive(Clone)]
pub struct PeerManagerConfig {
    target_num_for_peers: usize,
    blacklist_timeout: Duration,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum PeerManagerError {
    #[error("No such peer: {0}")]
    NoSuchPeer(PeerId),
    #[error("No such query: {0}")]
    NoSuchQuery(QueryId),
    #[error("Peer is blocked: {0}")]
    PeerIsBlocked(PeerId),
}

impl Default for PeerManagerConfig {
    fn default() -> Self {
        Self { target_num_for_peers: 100, blacklist_timeout: Duration::max_value() }
    }
}

#[allow(dead_code)]
impl<P> PeerManager<P>
where
    P: PeerTrait,
{
    fn new(config: PeerManagerConfig) -> Self {
        let peers = HashMap::new();
        Self {
            peers,
            query_to_peer_map: HashMap::new(),
            config,
            last_peer_index: 0,
            pending_events: Vec::new(),
            peer_pending_dial_with_events: HashMap::new(),
        }
    }

    fn add_peer(&mut self, mut peer: P) {
        peer.set_timeout_duration(self.config.blacklist_timeout);
        self.peers.insert(peer.peer_id(), peer);
    }

    #[cfg(test)]
    fn get_mut_peer(&mut self, peer_id: PeerId) -> Option<&mut P> {
        self.peers.get_mut(&peer_id)
    }

    fn assign_peer_to_query(&mut self, query_id: QueryId) -> Option<PeerId> {
        // TODO: consider moving this logic to be async (on a different tokio task)
        // until then we can return the assignment even if we use events for the notification.
        if self.peers.is_empty() {
            // TODO: how to handle this case with events? should we send an event for this?
            return None;
        }
        let peer = self
            .peers
            .iter()
            .skip(self.last_peer_index)
            .find(|(_, peer)| !peer.is_blocked())
            .or_else(|| {
                self.peers.iter().take(self.last_peer_index).find(|(_, peer)| !peer.is_blocked())
            });
        self.last_peer_index = (self.last_peer_index + 1) % self.peers.len();
        peer.map(|(peer_id, peer)| {
            // TODO: consider not allowing reassignment of the same query
            self.query_to_peer_map.insert(query_id, *peer_id);
            let event = ToSwarm::GenerateEvent(Event::NotifyStreamedBytes(
                streamed_bytes::behaviour::FromOtherBehaviour::QueryAssigned(query_id, *peer_id),
            ));
            if peer.connection_id().is_none() {
                // In case we have a race condition where the connection is closed after we added to
                // the pending list, the reciever will get an error and will need to ask for
                // re-assignment
                if let Some(events) = self.peer_pending_dial_with_events.get_mut(peer_id) {
                    events.push(event);
                } else {
                    self.peer_pending_dial_with_events.insert(*peer_id, vec![event]);
                }
                self.pending_events.push(ToSwarm::Dial {
                    opts: DialOpts::peer_id(*peer_id).addresses(vec![peer.multiaddr()]).build(),
                });
            } else {
                self.pending_events.push(event);
            }
            *peer_id
        })
    }

    fn report_peer(
        &mut self,
        peer_id: PeerId,
        reason: ReputationModifier,
    ) -> Result<(), PeerManagerError> {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.update_reputation(reason);
            Ok(())
        } else {
            Err(PeerManagerError::NoSuchPeer(peer_id))
        }
    }

    fn report_query(
        &mut self,
        query_id: QueryId,
        reason: ReputationModifier,
    ) -> Result<(), PeerManagerError> {
        if let Some(peer_id) = self.query_to_peer_map.get(&query_id) {
            if let Some(peer) = self.peers.get_mut(peer_id) {
                peer.update_reputation(reason);
                Ok(())
            } else {
                Err(PeerManagerError::NoSuchPeer(*peer_id))
            }
        } else {
            Err(PeerManagerError::NoSuchQuery(query_id))
        }
    }

    fn more_peers_needed(&self) -> bool {
        // TODO: consider if we should count blocked peers (and in what cases? what if they are
        // blocked temporarily?)
        self.peers.len() < self.config.target_num_for_peers
    }
}

impl From<Event> for mixed_behaviour::Event {
    fn from(event: Event) -> Self {
        match event {
            Event::NotifyStreamedBytes(event) => {
                Self::InternalEvent(mixed_behaviour::InternalEvent::NotifyStreamedBytes(event))
            }
            Event::NotifyDiscovery(event) => {
                Self::InternalEvent(mixed_behaviour::InternalEvent::NotifyDiscovery(event))
            }
        }
    }
}
use crate::discovery::Discovery;
use crate::rpc::{RPCEvent, RPCMessage, RPC};
use crate::{error, Enr, EnrForkId, SubnetId, NetworkConfig, NetworkGlobals, TopicHash, GossipTopic};
use crate::version;
use libp2p::{
    core::identity::Keypair,
    discv5::Discv5Event,
    gossipsub::{Gossipsub, GossipsubEvent, MessageId},
    identify::{Identify, IdentifyEvent},
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess},
    tokio_io::{AsyncRead, AsyncWrite},
    NetworkBehaviour, PeerId,
};
use futures::prelude::*;
use lru::LruCache;
use slog::{crit, debug, o, warn};
use std::sync::Arc;



const MAX_IDENTIFY_ADDRESSES: usize = 20;

/// Builds the network behaviour that manages the core protocols of eth2.
/// This core behaviour is managed by `Behaviour` which adds peer management to all core
/// behaviours.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "BehaviourEvent", poll_method = "poll")]
pub struct Behaviour<TSubstream: AsyncRead + AsyncWrite> {
    /// The routing pub-sub mechanism for eth2.
    gossipsub: Gossipsub<TSubstream>,
    /// The Eth2 RPC specified in the wire-0 protocol. TODO: fix
    rpc: RPC<TSubstream>,
    /// Keep regular connection to peers and disconnect if absent.
    // TODO: Using id for initial interop. This will be removed by mainnet.
    /// Provides IP addresses and peer information.
    identify: Identify<TSubstream>,
    /// Discovery behaviour
    discovery: Discovery<TSubstream>,
    /// The events generated by this behaviour to be consumed in the swarm poll.
    #[behaviour(ignore)]
    events: Vec<BehaviourEvent>,
    /// A cache of recently seen gossip messages. This is used to filter out any possible
    /// duplicates that may still be seen over gossipsub.
    #[behaviour(ignore)]
    seen_gossip_messages: LruCache<MessageId, ()>,
    /// A collections of variables accessible outside the network service.
    #[behaviour(ignore)]
    network_globals: Arc<NetworkGlobals>,
    /// Keeps track of the current EnrForkId for upgrading gossipsub topics.
    #[behaviour(ignore)]
    enr_fork_id: EnrForkId,
    /// Logger for behaviour actions.
    #[behaviour(ignore)]
    log: slog::Logger,
}

impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    pub fn new(
        local_key: &Keypair,
        net_conf: &NetworkConfig,
        network_globals: Arc<NetworkGlobals>,
        enr_fork_id: EnrForkId,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        let local_peer_id = local_key.public().into_peer_id();
        let behaviour_log = log.new(o!());

        let identify = Identify::new(
            "mothra/libp2p".into(),
            version::version(),
            local_key.public(),
        );

        Ok(Behaviour {
            rpc: RPC::new(log),
            gossipsub: Gossipsub::new(local_peer_id, net_conf.gs_config.clone()),
            discovery: Discovery::new(
                local_key,
                net_conf,
                enr_fork_id.clone(),
                network_globals.clone(),
                log,
            )?,
            identify,
            events: Vec::new(),
            seen_gossip_messages: LruCache::new(100_000),
            network_globals,
            enr_fork_id,
            log: behaviour_log,
        })
    }

    pub fn discovery(&self) -> &Discovery<TSubstream> {
        &self.discovery
    }

    pub fn gs(&self) -> &Gossipsub<TSubstream> {
        &self.gossipsub
    }
}

/// Implements the combined behaviour for the libp2p service.
impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    /* Pubsub behaviour functions */

    /// Subscribes to a gossipsub topic.
    pub fn subscribe(&mut self, topic: GossipTopic) -> bool {
        // update the network globals
        self.network_globals
            .gossipsub_subscriptions
            .write()
            .insert(topic.clone());

        let topic_str: String = topic.clone().into();
        debug!(self.log, "Subscribed to topic"; "topic" => topic_str);
        self.gossipsub.subscribe(topic.into())
    }

    /// Unsubscribe from a gossipsub topic.
    pub fn unsubscribe(&mut self, topic: GossipTopic) -> bool {
        // update the network globals
        self.network_globals
            .gossipsub_subscriptions
            .write()
            .remove(&topic);
        // unsubscribe from the topic
        self.gossipsub.unsubscribe(topic.into())
    }

    /// Publishes a list of messages on the pubsub (gossipsub) behaviour
    pub fn publish(&mut self, topics: Vec<GossipTopic>, message: Vec<u8>) {
        for topic in topics {
            self.gossipsub.publish(&topic.into(), message.clone());
        }
    }

    /// Forwards a message that is waiting in gossipsub's mcache. 
    pub fn propagate_message(&mut self, propagation_source: &PeerId, message_id: MessageId) {
        self.gossipsub
            .propagate_message(&message_id, propagation_source);
    }

    /* RPC behaviour functions */

    /// Sends an RPC Request/Response via the RPC protocol.
    pub fn send_rpc(&mut self, peer_id: PeerId, rpc_event: RPCEvent) {
        self.rpc.send_rpc(peer_id, rpc_event);
    }

    /* Discovery / Peer management functions */

    /// Notify discovery that the peer has been banned.
    pub fn peer_banned(&mut self, peer_id: PeerId) {
        self.discovery.peer_banned(peer_id);
    }

    /// Notify discovery that the peer has been unbanned.
    pub fn peer_unbanned(&mut self, peer_id: &PeerId) {
        self.discovery.peer_unbanned(peer_id);
    }

    /// Returns an iterator over all enr entries in the DHT.
    pub fn enr_entries(&mut self) -> impl Iterator<Item = &Enr> {
        self.discovery.enr_entries()
    }

    // /// Add an ENR to the routing table of the discovery mechanism.
    pub fn add_enr(&mut self, enr: Enr) {
        self.discovery.add_enr(enr);
    }

    /// Updates a subnet value to the ENR bitfield.
    ///
    /// The `value` is `true` if a subnet is being added and false otherwise.
    //TODO: revisit bc update_enr_bitfield requires ssz
    pub fn update_enr_subnet(&mut self, subnet_id: SubnetId, value: bool) {
        if let Err(e) = self.discovery.update_enr_bitfield(subnet_id, value) {
            crit!(self.log, "Could not update ENR bitfield"; "error" => e);
        }
    }

    /// A request to search for peers connected to a long-lived subnet.
    pub fn peers_request(&mut self, subnet_id: SubnetId) {
        self.discovery.peers_request(subnet_id);
    }

    /// Updates the local ENR's "eth2" field with the latest EnrForkId.
    //TODO: fix the fact that the fork digest isnt updated
    pub fn update_fork_version(&mut self, enr_fork_id: EnrForkId) {
        self.discovery.update_eth2_enr(enr_fork_id.clone());

        // unsubscribe from all gossip topics and re-subscribe to their new fork counterparts
        let subscribed_topics = self
            .network_globals
            .gossipsub_subscriptions
            .read()
            .iter()
            .cloned()
            .collect::<Vec<GossipTopic>>();

        //  unsubscribe from all topics
        for topic in &subscribed_topics {
            self.unsubscribe(topic.clone());
        }

        // re-subscribe modifying the fork version
        for topic in subscribed_topics {
           // *topic.digest() = enr_fork_id.fork_digest;
           //TODO: fix this
            self.subscribe(topic);
        }

        // update the local reference
        self.enr_fork_id = enr_fork_id;
    }
}

// Implement the NetworkBehaviourEventProcess trait so that we can derive NetworkBehaviour for Behaviour
impl<TSubstream: AsyncRead + AsyncWrite>
    NetworkBehaviourEventProcess<GossipsubEvent> for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message(propagation_source, id, gs_msg) => {
                // Note: We are keeping track here of the peer that sent us the message, not the
                // peer that originally published the message.
                if self.seen_gossip_messages.put(id.clone(), ()).is_none() {
                    self.events.push(BehaviourEvent::GossipMessage {
                        id,
                        source: propagation_source,
                        topics: gs_msg.topics,
                        message: gs_msg.data
                    });
                } else {
                     warn!(self.log, "A duplicate gossipsub message was received"; "message" => format!("{:?}", gs_msg));
                }
            }
            GossipsubEvent::Subscribed { peer_id, topic } => {
                self.events
                    .push(BehaviourEvent::PeerSubscribed(peer_id, topic));
            }
            GossipsubEvent::Unsubscribed { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite>
    NetworkBehaviourEventProcess<RPCMessage> for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: RPCMessage) {
        match event {
            RPCMessage::PeerDialed(peer_id) => {
                self.events.push(BehaviourEvent::PeerDialed(peer_id))
            }
            RPCMessage::PeerDisconnected(peer_id) => {
                self.events.push(BehaviourEvent::PeerDisconnected(peer_id))
            }
            RPCMessage::RPC(peer_id, rpc_event) => {
                self.events.push(BehaviourEvent::RPC(peer_id, rpc_event))
            }
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> Behaviour<TSubstream> {
    /// Consumes the events list when polled.
    fn poll<TBehaviourIn>(
        &mut self,
    ) -> Async<NetworkBehaviourAction<TBehaviourIn, BehaviourEvent>> {
        if !self.events.is_empty() {
            return Async::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)));
        }

        Async::NotReady
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<IdentifyEvent>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Received {
                peer_id,
                mut info,
                observed_addr,
            } => {
                if info.listen_addrs.len() > MAX_IDENTIFY_ADDRESSES {
                    debug!(
                        self.log,
                        "More than 20 addresses have been identified, truncating"
                    );
                    info.listen_addrs.truncate(MAX_IDENTIFY_ADDRESSES);
                }
                debug!(self.log, "Identified Peer"; "peer" => format!("{}", peer_id),
                "protocol_version" => info.protocol_version,
                "agent_version" => info.agent_version,
                "listening_ addresses" => format!("{:?}", info.listen_addrs),
                "observed_address" => format!("{:?}", observed_addr),
                "protocols" => format!("{:?}", info.protocols)
                );
            }
            IdentifyEvent::Sent { .. } => {}
            IdentifyEvent::Error { .. } => {}
        }
    }
}

impl<TSubstream: AsyncRead + AsyncWrite> NetworkBehaviourEventProcess<Discv5Event>
    for Behaviour<TSubstream>
{
    fn inject_event(&mut self, _event: Discv5Event) {
        // discv5 has no events to inject
    }
}

/// The types of events than can be obtained from polling the behaviour.
pub enum BehaviourEvent {
    /// A received RPC event and the peer that it was received from.
    RPC(PeerId, RPCEvent),
    /// We have completed an initial connection to a new peer.
    PeerDialed(PeerId),
    /// A peer has disconnected.
    PeerDisconnected(PeerId),
    /// A gossipsub message has been received.
    GossipMessage {
        /// The gossipsub message id. Used when propagating blocks after validation.
        id: MessageId,
        /// The peer from which we received this message, not the peer that published it.
        source: PeerId,
        /// The topics that this message was sent on.
        topics: Vec<TopicHash>,
        /// The message itself.
        message: Vec<u8>,
    },
    /// Subscribed to peer for given topic
    PeerSubscribed(PeerId, TopicHash),
}

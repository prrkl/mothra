use crate::peer_manager::{PeerManager, PeerManagerEvent};
use crate::rpc::*;
use crate::types::{EnrForkId, GossipKind, GossipTopic, SubnetId};

use crate::{error, Enr, NetworkConfig, NetworkGlobals, TopicHash};
use futures::prelude::*;
use handler::{BehaviourHandler, BehaviourHandlerIn, BehaviourHandlerOut, DelegateIn, DelegateOut};
use libp2p::{
    core::{
        connection::{ConnectedPoint, ConnectionId, ListenerId},
        identity::Keypair,
        Multiaddr,
    },
    gossipsub::{Gossipsub, GossipsubEvent, MessageId},
    identify::{Identify, IdentifyEvent},
    swarm::{
        NetworkBehaviour, NetworkBehaviourAction as NBAction, NotifyHandler, PollParameters,
        ProtocolsHandler,
    },
    PeerId,
};
use lru::LruCache;
use slog::{crit, debug, o};
use std::{
    marker::PhantomData,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

mod handler;

const MAX_IDENTIFY_ADDRESSES: usize = 10;

/// Builds the network behaviour that manages the core protocols of eth2.
/// This core behaviour is managed by `Behaviour` which adds peer management to all core
/// behaviours.
pub struct Behaviour {
    /// The routing pub-sub mechanism.
    gossipsub: Gossipsub,
    /// The RPC mechanism.
    mothra_rpc: RPC,
    /// Keep regular connection to peers and disconnect if absent.
    // TODO: Using id for initial interop. This will be removed by mainnet.
    /// Provides IP addresses and peer information.
    identify: Identify,
    /// The peer manager that keeps track of peer's reputation and status.
    peer_manager: PeerManager,
    /// The events generated by this behaviour to be consumed in the swarm poll.
    events: Vec<BehaviourEvent>,
    /// Queue of peers to disconnect.
    peers_to_dc: Vec<PeerId>,
    /// The current meta data of the node
    meta_data: Vec<u8>,
    /// The current ping data of the node
    ping_data: Vec<u8>,
    /// A cache of recently seen gossip messages. This is used to filter out any possible
    /// duplicates that may still be seen over gossipsub.
    // TODO: Remove this
    seen_gossip_messages: LruCache<MessageId, ()>,
    /// A collections of variables accessible outside the network service.
    network_globals: Arc<NetworkGlobals>,
    /// Keeps track of the current EnrForkId for upgrading gossipsub topics.
    // NOTE: This can be accessed via the network_globals ENR. However we keep it here for quick
    // lookups for every gossipsub message send.
    enr_fork_id: EnrForkId,
    /// Logger for behaviour actions.
    log: slog::Logger,
}

/// Calls the given function with the given args on all sub behaviours.
macro_rules! delegate_to_behaviours {
    ($self: ident, $fn: ident, $($arg: ident), *) => {
        $self.gossipsub.$fn($($arg),*);
        $self.mothra_rpc.$fn($($arg),*);
        $self.identify.$fn($($arg),*);
    };
}

impl NetworkBehaviour for Behaviour {
    type ProtocolsHandler = BehaviourHandler;
    type OutEvent = BehaviourEvent;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        BehaviourHandler::new(
            &mut self.gossipsub,
            &mut self.mothra_rpc,
            &mut self.identify,
        )
    }

    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        self.peer_manager.addresses_of_peer(peer_id)
    }

    fn inject_connected(&mut self, peer_id: &PeerId) {
        delegate_to_behaviours!(self, inject_connected, peer_id);
    }

    fn inject_disconnected(&mut self, peer_id: &PeerId) {
        delegate_to_behaviours!(self, inject_disconnected, peer_id);
    }

    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        conn_id: &ConnectionId,
        endpoint: &ConnectedPoint,
    ) {
        delegate_to_behaviours!(
            self,
            inject_connection_established,
            peer_id,
            conn_id,
            endpoint
        );
    }

    fn inject_connection_closed(
        &mut self,
        peer_id: &PeerId,
        conn_id: &ConnectionId,
        endpoint: &ConnectedPoint,
    ) {
        delegate_to_behaviours!(self, inject_connection_closed, peer_id, conn_id, endpoint);
    }

    fn inject_addr_reach_failure(
        &mut self,
        peer_id: Option<&PeerId>,
        addr: &Multiaddr,
        error: &dyn std::error::Error,
    ) {
        delegate_to_behaviours!(self, inject_addr_reach_failure, peer_id, addr, error);
    }

    fn inject_dial_failure(&mut self, peer_id: &PeerId) {
        delegate_to_behaviours!(self, inject_dial_failure, peer_id);
    }

    fn inject_new_listen_addr(&mut self, addr: &Multiaddr) {
        delegate_to_behaviours!(self, inject_new_listen_addr, addr);
    }

    fn inject_expired_listen_addr(&mut self, addr: &Multiaddr) {
        delegate_to_behaviours!(self, inject_expired_listen_addr, addr);
    }

    fn inject_new_external_addr(&mut self, addr: &Multiaddr) {
        delegate_to_behaviours!(self, inject_new_external_addr, addr);
    }

    fn inject_listener_error(&mut self, id: ListenerId, err: &(dyn std::error::Error + 'static)) {
        delegate_to_behaviours!(self, inject_listener_error, id, err);
    }
    fn inject_listener_closed(&mut self, id: ListenerId, reason: Result<(), &std::io::Error>) {
        delegate_to_behaviours!(self, inject_listener_closed, id, reason);
    }

    fn inject_event(
        &mut self,
        peer_id: PeerId,
        conn_id: ConnectionId,
        event: <Self::ProtocolsHandler as ProtocolsHandler>::OutEvent,
    ) {
        match event {
            // Events comming from the handler, redirected to each behaviour
            BehaviourHandlerOut::Delegate(delegate) => match *delegate {
                DelegateOut::Gossipsub(ev) => self.gossipsub.inject_event(peer_id, conn_id, ev),
                DelegateOut::RPC(ev) => self.mothra_rpc.inject_event(peer_id, conn_id, ev),
                DelegateOut::Identify(ev) => self.identify.inject_event(peer_id, conn_id, *ev),
            },
            /* Custom events sent BY the handler */
            BehaviourHandlerOut::Custom => {
                // TODO: implement
            }
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context,
        poll_params: &mut impl PollParameters,
    ) -> Poll<NBAction<<Self::ProtocolsHandler as ProtocolsHandler>::InEvent, Self::OutEvent>> {
        // TODO: move where it's less distracting
        macro_rules! poll_behaviour {
            /* $behaviour:  The sub-behaviour being polled.
             * $on_event_fn:  Function to call if we get an event from the sub-behaviour.
             * $notify_handler_event_closure:  Closure mapping the received event type to
             *     the one that the handler should get.
             */
            ($behaviour: ident, $on_event_fn: ident, $notify_handler_event_closure: expr) => {
                loop {
                    // poll the sub-behaviour
                    match self.$behaviour.poll(cx, poll_params) {
                        Poll::Ready(action) => match action {
                            // call the designated function to handle the event from sub-behaviour
                            NBAction::GenerateEvent(event) => self.$on_event_fn(event),
                            NBAction::DialAddress { address } => {
                                return Poll::Ready(NBAction::DialAddress { address })
                            }
                            NBAction::DialPeer { peer_id, condition } => {
                                return Poll::Ready(NBAction::DialPeer { peer_id, condition })
                            }
                            NBAction::NotifyHandler {
                                peer_id,
                                handler,
                                event,
                            } => {
                                return Poll::Ready(NBAction::NotifyHandler {
                                    peer_id,
                                    handler,
                                    // call the closure mapping the received event to the needed one
                                    // in order to notify the handler
                                    event: BehaviourHandlerIn::Delegate(
                                        $notify_handler_event_closure(event),
                                    ),
                                });
                            }
                            NBAction::ReportObservedAddr { address } => {
                                return Poll::Ready(NBAction::ReportObservedAddr { address })
                            }
                        },
                        Poll::Pending => break,
                    }
                }
            };
        }

        poll_behaviour!(gossipsub, on_gossip_event, DelegateIn::Gossipsub);
        poll_behaviour!(mothra_rpc, on_rpc_event, DelegateIn::RPC);
        poll_behaviour!(identify, on_identify_event, DelegateIn::Identify);

        self.custom_poll(cx)
    }
}

/// Implements the combined behaviour for the libp2p service.
impl Behaviour {
    pub fn new(
        local_key: &Keypair,
        config: &NetworkConfig,
        network_globals: Arc<NetworkGlobals>,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        let local_peer_id = local_key.public().into_peer_id();
        let behaviour_log = log.new(o!());

        let identify = Identify::new(
            config.protocol_version.clone(),
            config.agent_version.clone(),
            local_key.public(),
        );

        let enr_fork_id = network_globals.local_fork_id();

        let meta_data = network_globals.meta_data.read().clone();

        let ping_data = network_globals.ping_data.read().clone();

        Ok(Behaviour {
            mothra_rpc: RPC::new(log.clone()),
            gossipsub: Gossipsub::new(local_peer_id, config.gs_config.clone()),
            identify,
            peer_manager: PeerManager::new(local_key, config, network_globals.clone(), log)?,
            events: Vec::new(),
            peers_to_dc: Vec::new(),
            seen_gossip_messages: LruCache::new(100_000),
            meta_data,
            ping_data,
            network_globals,
            enr_fork_id,
            log: behaviour_log,
        })
    }

    /// Returns the local ENR of the node.
    pub fn local_enr(&self) -> Enr {
        self.network_globals.local_enr()
    }

    /// Obtain a reference to the gossipsub protocol.
    pub fn gs(&self) -> &Gossipsub {
        &self.gossipsub
    }

    /* Pubsub behaviour functions */

    /// Subscribes to a gossipsub topic kind
    pub fn subscribe_kind(&mut self, kind: GossipKind) -> bool {
        let gossip_topic = GossipTopic::new(kind);
        self.subscribe(gossip_topic)
    }

    /// Unsubscribes from a gossipsub topic kind
    pub fn unsubscribe_kind(&mut self, kind: GossipKind) -> bool {
        let gossip_topic = GossipTopic::new(kind);
        self.unsubscribe(gossip_topic)
    }

    /// Subscribes to a gossipsub topic.
    fn subscribe(&mut self, topic: GossipTopic) -> bool {
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
    fn unsubscribe(&mut self, topic: GossipTopic) -> bool {
        // update the network globals
        self.network_globals
            .gossipsub_subscriptions
            .write()
            .remove(&topic);
        // unsubscribe from the topic
        self.gossipsub.unsubscribe(topic.into())
    }

    /// Publishes a list of messages on the pubsub (gossipsub) behaviour, choosing the encoding.
    pub fn publish(&mut self, topic: GossipTopic, message: Vec<u8>) {
        self.gossipsub.publish(&topic.into(), message);
    }

    /// Forwards a message that is waiting in gossipsub's mcache. Messages are only propagated
    /// once validated by the beacon chain.
    pub fn propagate_message(&mut self, propagation_source: &PeerId, message_id: MessageId) {
        self.gossipsub
            .propagate_message(&message_id, propagation_source);
    }

    /// Send a request to a peer over RPC.
    pub fn send_request(&mut self, peer_id: PeerId, request_id: RequestId, request: Request) {
        self.mothra_rpc.send_request(peer_id, request_id, request.into());
    }

    /// Send a successful response to a peer over RPC.
    pub fn send_successful_response(
        &mut self,
        peer_id: PeerId,
        id: PeerRequestId,
        response: Response,
    ) {
        self.mothra_rpc.send_response(peer_id, id, response.into())
    }

    /// Inform the peer that their request produced an error.
    pub fn _send_error_reponse(
        &mut self,
        peer_id: PeerId,
        id: PeerRequestId,
        error: RPCResponseErrorCode,
        reason: String,
    ) {
        self.mothra_rpc.send_response(
            peer_id,
            id,
            RPCCodedResponse::from_error_code(error, reason),
        )
    }

    /* Peer management functions */

    /// Notify discovery that the peer has been banned.
    // TODO: Remove this and integrate all disconnection/banning logic inside the peer manager.
    pub fn peer_banned(&mut self, _peer_id: PeerId) {}

    /// Notify discovery that the peer has been unbanned.
    // TODO: Remove this and integrate all disconnection/banning logic inside the peer manager.
    pub fn peer_unbanned(&mut self, _peer_id: &PeerId) {}

    /// Returns an iterator over all enr entries in the DHT.
    pub fn enr_entries(&mut self) -> Vec<Enr> {
        self.peer_manager.discovery_mut().table_entries_enr()
    }

    /// Add an ENR to the routing table of the discovery mechanism.
    pub fn add_enr(&mut self, enr: Enr) {
        self.peer_manager.discovery_mut().add_enr(enr);
    }

    /// Attempts to discover new peers for a given subnet. The `min_ttl` gives the time at which we
    /// would like to retain the peers for.
    pub fn discover_subnet_peers(&mut self, subnet_id: SubnetId, min_ttl: Option<Instant>) {
        //TODO: not sure yet
        //self.peer_manager.discover_subnet_peers(subnet_id, min_ttl)
    }

    /* Private internal functions */

    /// Updates the current meta data of the node to match the local ENR.
    fn update_metadata(&mut self) {
        //TODO: JR Add ability to update
        //self.meta_data.seq_number += 1;
        //self.meta_data.attnets = vec![];
    }

    /// Sends a Ping request to the peer.
    fn ping(&mut self, id: RequestId, peer_id: PeerId) {
        debug!(self.log, "Sending Ping"; "request_id" => id, "peer_id" => peer_id.to_string());

        self.mothra_rpc
           .send_request(peer_id, id, RPCRequest::Ping(self.ping_data.clone()));
    }

    /// Sends a Pong response to the peer.
    fn pong(&mut self, id: PeerRequestId, peer_id: PeerId) {

        debug!(self.log, "Sending Pong"; "request_id" => id.1, "peer_id" => peer_id.to_string());
        let event = RPCCodedResponse::Success(RPCResponse::Pong(self.ping_data.clone()));
        self.mothra_rpc.send_response(peer_id, id, event);
    }

    /// Sends a METADATA request to a peer.
    fn send_meta_data_request(&mut self, peer_id: PeerId) {
        debug!(self.log, "Sending MetaData request"; "peer_id" => peer_id.to_string());
        let event = RPCRequest::MetaData;
        self.mothra_rpc
            .send_request(peer_id, RequestId::Behaviour, event);
    }

    /// Sends a METADATA response to a peer.
    fn send_meta_data_response(&mut self, id: PeerRequestId, peer_id: PeerId) {
        debug!(self.log, "Sending MetaData response"; "peer_id" => peer_id.to_string());
        let event = RPCCodedResponse::Success(RPCResponse::MetaData(self.meta_data.clone()));
        self.mothra_rpc.send_response(peer_id, id, event);
    }

    /// Returns a reference to the peer manager to allow the swarm to notify the manager of peer
    /// status
    pub fn peer_manager(&mut self) -> &mut PeerManager {
        &mut self.peer_manager
    }

    fn on_gossip_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message(propagation_source, id, gs_msg) => {
                //LRU logic should be implemented in client
                self.events.push(BehaviourEvent::PubsubMessage {
                    id,
                    source: propagation_source,
                    topics: gs_msg.topics,
                    message: gs_msg.data,
                });
            }
            GossipsubEvent::Subscribed { peer_id, topic } => {
                self.events
                    .push(BehaviourEvent::PeerSubscribed(peer_id, topic));
            }
            GossipsubEvent::Unsubscribed { .. } => {}
        }
    }

    /// Queues the response to be sent upwards as long at it was requested outside the Behaviour.
    fn propagate_response(&mut self, id: RequestId, peer_id: PeerId, response: Response) {
        if !matches!(id, RequestId::Behaviour) {
            self.events.push(BehaviourEvent::ResponseReceived {
                peer_id,
                id,
                response,
            });
        }
    }

    /// Convenience function to propagate a request.
    fn propagate_request(&mut self, id: PeerRequestId, peer_id: PeerId, request: Request) {
        self.events.push(BehaviourEvent::RequestReceived {
            peer_id,
            id,
            request,
        });
    }

    fn on_rpc_event(&mut self, message: RPCMessage) {
        let peer_id = message.peer_id;
        let handler_id = message.conn_id;
        // The METADATA and PING RPC responses are handled within the behaviour and not propagated
        match message.event {
            Err(handler_err) => {
                match handler_err {
                    HandlerErr::Inbound {
                        id: _,
                        proto,
                        error,
                    } => {
                        if matches!(error, RPCError::HandlerRejected) {
                            // this peer's request got canceled
                            // TODO: cancel processing for this request
                        }
                        // Inform the peer manager of the error.
                        // An inbound error here means we sent an error to the peer, or the stream
                        // timed out.
                        self.peer_manager.handle_rpc_error(&peer_id, proto, &error);
                    }
                    HandlerErr::Outbound { id, proto, error } => {
                        // Inform the peer manager that a request we sent to the peer failed
                        self.peer_manager.handle_rpc_error(&peer_id, proto, &error);
                        // inform failures of requests comming outside the behaviour
                        if !matches!(id, RequestId::Behaviour) {
                            self.events
                                .push(BehaviourEvent::RPCFailed { peer_id, id, error });
                        }
                    }
                }
            }
            Ok(RPCReceived::Request(id, request)) => {
                let peer_request_id = (handler_id, id);
                match request {
                    /* Behaviour managed protocols: Ping and Metadata */
                    RPCRequest::Ping(ping) => {
                        // inform the peer manager and send the response
                        debug!(self.log, "Behaviour RPCRequest::Ping received from: {:?}", peer_id);
                        //TODO: JR - peer manager won't be properly updated until i serialize externally
                        //self.peer_manager.ping_request(&peer_id, ping.data);
                        // send a ping response
                        self.pong(peer_request_id, peer_id);
                    }
                    RPCRequest::MetaData => {
                        // send the requested meta-data
                        self.send_meta_data_response((handler_id, id), peer_id);
                        // TODO: inform the peer manager?
                    }
                    RPCRequest::Goodbye(reason) => {
                        // let the peer manager know this peer is in the process of disconnecting
                        self.peer_manager._disconnecting_peer(&peer_id);
                        // queue for disconnection without a goodbye message
                        debug!(self.log, "Behaviour received a Goodbye, queueing for disconnection";
                            "peer_id" => peer_id.to_string());
                        self.peers_to_dc.push(peer_id.clone());
                        // TODO: do not propagate (Age comment)
                        //TODO: JR raise event to decode before calling propagate_request
                        //self.propagate_request(peer_request_id, peer_id, Request::Goodbye(reason));
                    }
                    /* Protocols propagated to the Network */
                    RPCRequest::Status(msg) => {
                        debug!(self.log, "Behaviour RPCRequest::Status received from: {:?}", peer_id);
                        // inform the peer manager that we have received a status from a peer
                        self.peer_manager.peer_statusd(&peer_id);
                        // propagate the STATUS message upwards
                        self.propagate_request(peer_request_id, peer_id, Request::Status(msg))
                    }
                    _ => (),
                }
            }
            Ok(RPCReceived::Response(id, resp)) => {
                match resp {
                    /* Behaviour managed protocols */
                    RPCResponse::Pong(ping) => {
                        debug!(self.log, "Behaviour RPCResponse::Pong received from: {:?}", peer_id);
                        //TODO: JR - raise event to decode
                        //self.peer_manager.pong_response(&peer_id, ping.data)
                    }
                    RPCResponse::MetaData(meta_data) => {
                        debug!(self.log, "Behaviour RPCResponse::MetaData received from: {:?}", peer_id);
                        //self.peer_manager.meta_data_response(&peer_id, meta_data)
                    }
                    /* Network propagated protocols */
                    RPCResponse::Status(msg) => {
                        debug!(self.log, "Behaviour RPCResponse::Status received from: {:?}", peer_id);
                        // inform the peer manager that we have received a status from a peer
                        self.peer_manager.peer_statusd(&peer_id);
                        // propagate the STATUS message upwards
                        //TODO: JR- raise event to decode
                        self.propagate_response(id, peer_id, Response::Status(msg));
                    }
                    _ => (),
                }
            }
        }
    }

    /// Consumes the events list when polled.
    fn custom_poll(
        &mut self,
        cx: &mut Context,
    ) -> Poll<NBAction<BehaviourHandlerIn, BehaviourEvent>> {
        // handle pending disconnections to perform
        if !self.peers_to_dc.is_empty() {
            return Poll::Ready(NBAction::NotifyHandler {
                peer_id: self.peers_to_dc.remove(0),
                handler: NotifyHandler::All,
                event: BehaviourHandlerIn::Shutdown(None),
            });
        }

        // check the peer manager for events
        loop {
            match self.peer_manager.poll_next_unpin(cx) {
                Poll::Ready(Some(event)) => match event {
                    PeerManagerEvent::Dial(peer_id) => {
                        return Poll::Ready(NBAction::DialPeer {
                            peer_id,
                            condition: libp2p::swarm::DialPeerCondition::Disconnected,
                        });
                    }
                    PeerManagerEvent::SocketUpdated(address) => {
                        return Poll::Ready(NBAction::ReportObservedAddr { address });
                    }
                    PeerManagerEvent::Status(peer_id) => {
                        // it's time to status. We don't keep a beacon chain reference here, so we inform
                        // the network to send a status to this peer
                        return Poll::Ready(NBAction::GenerateEvent(BehaviourEvent::StatusPeer(
                            peer_id,
                        )));
                    }
                    PeerManagerEvent::Ping(peer_id) => {
                        // send a ping request to this peer
                        self.ping(RequestId::Behaviour, peer_id);
                    }
                    PeerManagerEvent::MetaData(peer_id) => {
                        self.send_meta_data_request(peer_id);
                    }
                    PeerManagerEvent::DisconnectPeer(peer_id) => {
                        debug!(self.log, "PeerManager requested to disconnect a peer";
                            "peer_id" => peer_id.to_string());
                        // queue for disabling
                        self.peers_to_dc.push(peer_id.clone());
                        // send one goodbye
                        return Poll::Ready(NBAction::NotifyHandler {
                            peer_id,
                            handler: NotifyHandler::Any,
                            event: BehaviourHandlerIn::Shutdown(Some((
                                RequestId::Behaviour,
                                RPCRequest::Goodbye(vec![]),
                                //RPCRequest::Goodbye(GoodbyeReason::Fault),
                            ))),
                        });
                    }
                },
                Poll::Pending => break,
                Poll::Ready(None) => break, // peer manager ended
            }
        }

        if !self.events.is_empty() {
            return Poll::Ready(NBAction::GenerateEvent(self.events.remove(0)));
        }

        Poll::Pending
    }

    fn on_identify_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Received {
                peer_id,
                mut info,
                observed_addr,
            } => {
                if info.listen_addrs.len() > MAX_IDENTIFY_ADDRESSES {
                    debug!(
                        self.log,
                        "More than 10 addresses have been identified, truncating"
                    );
                    info.listen_addrs.truncate(MAX_IDENTIFY_ADDRESSES);
                }
                // send peer info to the peer manager.
                self.peer_manager.identify(&peer_id, &info);

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

/* Public API types */

/// The type of RPC requests the Behaviour informs it has received and allows for sending.
///
// NOTE: This is an application-level wrapper over the lower network leve requests that can be
//       sent. The main difference is the absense of the Ping and Metadata protocols, which don't
//       leave the Behaviour. For all protocols managed by RPC see `RPCRequest`.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// A Status message.
    Status(Vec<u8>),
    /// A Goobye message.
    Goodbye(Vec<u8>),
}


impl std::convert::From<Request> for RPCRequest {
    fn from(req: Request) -> RPCRequest {
        match req {
            Request::Goodbye(r) => RPCRequest::Goodbye(r),
            Request::Status(s) => RPCRequest::Status(s),
        }
    }
}

/// The type of RPC responses the Behaviour informs it has received, and allows for sending.
///
// NOTE: This is an application-level wrapper over the lower network level responses that can be
//       sent. The main difference is the absense of Pong and Metadata, which don't leave the
//       Behaviour. For all protocol reponses managed by RPC see `RPCResponse` and
//       `RPCCodedResponse`.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// A Status message.
    Status(Vec<u8>),
}

//TODO: not sure yet
impl std::convert::From<Response> for RPCCodedResponse {
    fn from(resp: Response) -> RPCCodedResponse {
        match resp {
            Response::Status(s) => RPCCodedResponse::Success(RPCResponse::Status(s)),
        }
    }
}

/// Identifier of requests sent by a peer.
pub type PeerRequestId = (ConnectionId, SubstreamId);

/// The types of events than can be obtained from polling the behaviour.
#[derive(Debug)]
pub enum BehaviourEvent {
    /// An RPC Request that was sent failed.
    RPCFailed {
        /// The id of the failed request.
        id: RequestId,
        /// The peer to which this request was sent.
        peer_id: PeerId,
        /// The error that occurred.
        error: RPCError,
    },
    RequestReceived {
        /// The peer that sent the request.
        peer_id: PeerId,
        /// Identifier of the request. All responses to this request must use this id.
        id: PeerRequestId,
        /// Request the peer sent.
        request: Request,
    },
    ResponseReceived {
        /// Peer that sent the response.
        peer_id: PeerId,
        /// Id of the request to which the peer is responding.
        id: RequestId,
        /// Response the peer sent.
        response: Response,
    },
    PubsubMessage {
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
    /// Inform the network to send a Status to this peer.
    StatusPeer(PeerId),
}

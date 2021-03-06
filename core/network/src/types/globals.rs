//! A collection of variables that are accessible outside of the network thread itself.
use crate::rpc::methods::MetaData;
use crate::Client;
use crate::EnrExt;
use crate::{Enr, EnrForkId, GossipTopic, Multiaddr, PeerDB, PeerId};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU16, Ordering};

pub struct NetworkGlobals {
    /// The current local ENR.
    pub local_enr: RwLock<Enr>,
    /// The current local fork_id
    pub fork_id: RwLock<EnrForkId>,
    /// The current node's meta-data.
    pub meta_data: RwLock<Vec<u8>>,
    /// The current node's ping-data.
    pub ping_data: RwLock<Vec<u8>>,
    /// The local peer_id.
    pub peer_id: RwLock<PeerId>,
    /// Listening multiaddrs.
    pub listen_multiaddrs: RwLock<Vec<Multiaddr>>,
    /// The tcp port that the libp2p service is listening on
    pub listen_port_tcp: AtomicU16,
    /// The udp port that the discovery service is listening on
    pub listen_port_udp: AtomicU16,
    /// The collection of known peers.
    pub peers: RwLock<PeerDB>,
    /// The current gossipsub topic subscriptions.
    pub gossipsub_subscriptions: RwLock<HashSet<GossipTopic>>,
}

impl NetworkGlobals {
    pub fn new(
        enr: Enr,
        fork_id: EnrForkId,
        meta_data: Vec<u8>,
        ping_data: Vec<u8>,
        tcp_port: u16,
        udp_port: u16,
        log: &slog::Logger,
    ) -> Self {
        NetworkGlobals {
            local_enr: RwLock::new(enr.clone()),
            fork_id: RwLock::new(fork_id.clone()),
            meta_data: RwLock::new(meta_data),
            ping_data: RwLock::new(ping_data),
            peer_id: RwLock::new(enr.peer_id()),
            listen_multiaddrs: RwLock::new(Vec::new()),
            listen_port_tcp: AtomicU16::new(tcp_port),
            listen_port_udp: AtomicU16::new(udp_port),
            peers: RwLock::new(PeerDB::new(log)),
            gossipsub_subscriptions: RwLock::new(HashSet::new()),
        }
    }

    /// Returns the local ENR from the underlying Discv5 behaviour that external peers may connect
    /// to.
    pub fn local_enr(&self) -> Enr {
        self.local_enr.read().clone()
    }

    /// Returns the local fork_id from the underlying Discv5 behaviour that external peers may connect
    /// to.
    pub fn local_fork_id(&self) -> EnrForkId {
        self.fork_id.read().clone()
    }

    /// Returns the local libp2p PeerID.
    pub fn local_peer_id(&self) -> PeerId {
        self.peer_id.read().clone()
    }

    /// Returns the list of `Multiaddr` that the underlying libp2p instance is listening on.
    pub fn listen_multiaddrs(&self) -> Vec<Multiaddr> {
        self.listen_multiaddrs.read().clone()
    }

    /// Returns the libp2p TCP port that this node has been configured to listen on.
    pub fn listen_port_tcp(&self) -> u16 {
        self.listen_port_tcp.load(Ordering::Relaxed)
    }

    /// Returns the UDP discovery port that this node has been configured to listen on.
    pub fn listen_port_udp(&self) -> u16 {
        self.listen_port_udp.load(Ordering::Relaxed)
    }

    /// Returns the number of libp2p connected peers.
    pub fn connected_peers(&self) -> usize {
        self.peers.read().connected_peer_ids().count()
    }

    /// Returns the number of libp2p peers that are either connected or being dialed.
    pub fn connected_or_dialing_peers(&self) -> usize {
        self.peers.read().connected_or_dialing_peers().count()
    }

    /// Returns a `Client` type if one is known for the `PeerId`.
    pub fn client(&self, peer_id: &PeerId) -> Client {
        self.peers
            .read()
            .peer_info(peer_id)
            .map(|info| info.client.clone())
            .unwrap_or_default()
    }
}

//! Implementation of Lighthouse's peer management system.

pub use self::peerdb::*;
use crate::discovery::TARGET_SUBNET_PEERS;
use crate::rpc::{GoodbyeReason, MetaData, Protocol, RPCError, RPCResponseErrorCode};
use crate::types::SyncState;
use crate::{error, metrics, Gossipsub};
use crate::{NetworkConfig, NetworkGlobals, PeerId};
use crate::{Subnet, SubnetDiscovery};
use discv5::Enr;
use futures::prelude::*;
use futures::Stream;
use hashset_delay::HashSetDelay;
use libp2p::core::ConnectedPoint;
use libp2p::identify::IdentifyInfo;
use slog::{crit, debug, error, warn};
use smallvec::SmallVec;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use types::{EthSpec, SyncSubnetId};

pub use libp2p::core::{identity::Keypair, Multiaddr};

pub mod client;
mod peer_info;
mod peer_sync_status;
#[allow(clippy::mutable_key_type)] // PeerId in hashmaps are no longer permitted by clippy
mod peerdb;
pub(crate) mod score;

pub use peer_info::{ConnectionDirection, PeerConnectionStatus, PeerConnectionStatus::*, PeerInfo};
pub use peer_sync_status::{PeerSyncStatus, SyncInfo};
use score::{PeerAction, ReportSource, ScoreState};
use std::cmp::Ordering;
use std::collections::{hash_map::Entry, HashMap};
use std::net::IpAddr;

/// The time in seconds between re-status's peers.
const STATUS_INTERVAL: u64 = 300;
/// The time in seconds between PING events. We do not send a ping if the other peer has PING'd us
/// within this time frame (Seconds)
/// This is asymmetric to avoid simultaneous pings.
/// The interval for outbound connections.
const PING_INTERVAL_OUTBOUND: u64 = 15;
/// The interval for inbound connections.
const PING_INTERVAL_INBOUND: u64 = 20;

/// The heartbeat performs regular updates such as updating reputations and performing discovery
/// requests. This defines the interval in seconds.
const HEARTBEAT_INTERVAL: u64 = 30;

/// A fraction of `PeerManager::target_peers` that we allow to connect to us in excess of
/// `PeerManager::target_peers`. For clarity, if `PeerManager::target_peers` is 50 and
/// PEER_EXCESS_FACTOR = 0.1 we allow 10% more nodes, i.e 55.
pub const PEER_EXCESS_FACTOR: f32 = 0.1;
/// A fraction of `PeerManager::target_peers` that need to be outbound-only connections.
pub const MIN_OUTBOUND_ONLY_FACTOR: f32 = 0.3;
/// The fraction of extra peers beyond the PEER_EXCESS_FACTOR that we allow us to dial for when
/// requiring subnet peers. More specifically, if our target peer limit is 50, and our excess peer
/// limit is 55, and we are at 55 peers, the following parameter provisions a few more slots of
/// dialing priority peers we need for validator duties.
pub const PRIORITY_PEER_EXCESS: f32 = 0.05;

/// Relative factor of peers that are allowed to have a negative gossipsub score without penalizing
/// them in lighthouse.
const ALLOWED_NEGATIVE_GOSSIPSUB_FACTOR: f32 = 0.1;

/// The main struct that handles peer's reputation and connection status.
pub struct PeerManager<TSpec: EthSpec> {
    /// Storage of network globals to access the `PeerDB`.
    network_globals: Arc<NetworkGlobals<TSpec>>,
    /// A queue of events that the `PeerManager` is waiting to produce.
    events: SmallVec<[PeerManagerEvent; 16]>,
    /// A collection of inbound-connected peers awaiting to be Ping'd.
    inbound_ping_peers: HashSetDelay<PeerId>,
    /// A collection of outbound-connected peers awaiting to be Ping'd.
    outbound_ping_peers: HashSetDelay<PeerId>,
    /// A collection of peers awaiting to be Status'd.
    status_peers: HashSetDelay<PeerId>,
    /// The target number of peers we would like to connect to.
    target_peers: usize,
    /// A collection of sync committee subnets that we need to stay subscribed to.
    /// Sync committee subnets are longer term (256 epochs). Hence, we need to re-run
    /// discovery queries for subnet peers if we disconnect from existing sync
    /// committee subnet peers.
    sync_committee_subnets: HashMap<SyncSubnetId, Instant>,
    /// The heartbeat interval to perform routine maintenance.
    heartbeat: tokio::time::Interval,
    /// Keeps track of whether the discovery service is enabled or not.
    discovery_enabled: bool,
    /// The logger associated with the `PeerManager`.
    log: slog::Logger,
}

/// The events that the `PeerManager` outputs (requests).
pub enum PeerManagerEvent {
    /// A peer has dialed us.
    PeerConnectedIncoming(PeerId),
    /// A peer has been dialed.
    PeerConnectedOutgoing(PeerId),
    /// A peer has disconnected.
    PeerDisconnected(PeerId),
    /// Sends a STATUS to a peer.
    Status(PeerId),
    /// Sends a PING to a peer.
    Ping(PeerId),
    /// Request METADATA from a peer.
    MetaData(PeerId),
    /// The peer should be disconnected.
    DisconnectPeer(PeerId, GoodbyeReason),
    /// Inform the behaviour to ban this peer and associated ip addresses.
    Banned(PeerId, Vec<IpAddr>),
    /// The peer should be unbanned with the associated ip addresses.
    UnBanned(PeerId, Vec<IpAddr>),
    /// Request the behaviour to discover more peers.
    DiscoverPeers,
    /// Request the behaviour to discover peers on subnets.
    DiscoverSubnetPeers(Vec<SubnetDiscovery>),
}

impl<TSpec: EthSpec> PeerManager<TSpec> {
    // NOTE: Must be run inside a tokio executor.
    pub async fn new(
        config: &NetworkConfig,
        network_globals: Arc<NetworkGlobals<TSpec>>,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        // Set up the peer manager heartbeat interval
        let heartbeat = tokio::time::interval(tokio::time::Duration::from_secs(HEARTBEAT_INTERVAL));

        Ok(PeerManager {
            network_globals,
            events: SmallVec::new(),
            inbound_ping_peers: HashSetDelay::new(Duration::from_secs(PING_INTERVAL_INBOUND)),
            outbound_ping_peers: HashSetDelay::new(Duration::from_secs(PING_INTERVAL_OUTBOUND)),
            status_peers: HashSetDelay::new(Duration::from_secs(STATUS_INTERVAL)),
            target_peers: config.target_peers,
            sync_committee_subnets: Default::default(),
            heartbeat,
            discovery_enabled: !config.disable_discovery,
            log: log.clone(),
        })
    }

    /* Public accessible functions */

    /// The application layer wants to disconnect from a peer for a particular reason.
    ///
    /// All instant disconnections are fatal and we ban the associated peer.
    ///
    /// This will send a goodbye and disconnect the peer if it is connected or dialing.
    pub fn goodbye_peer(&mut self, peer_id: &PeerId, reason: GoodbyeReason, source: ReportSource) {
        // get the peer info
        if let Some(info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            debug!(self.log, "Sending goodbye to peer"; "peer_id" => %peer_id, "reason" => %reason, "score" => %info.score());
            if matches!(reason, GoodbyeReason::IrrelevantNetwork) {
                info.sync_status.update(PeerSyncStatus::IrrelevantPeer);
            }

            // Goodbye's are fatal
            info.apply_peer_action_to_score(PeerAction::Fatal);
            metrics::inc_counter_vec(
                &metrics::PEER_ACTION_EVENTS_PER_CLIENT,
                &[
                    info.client.kind.as_ref(),
                    PeerAction::Fatal.as_ref(),
                    source.into(),
                ],
            );
        }

        // Update the peerdb and start the disconnection.
        self.ban_peer(peer_id, reason);
    }

    /// Reports a peer for some action.
    ///
    /// If the peer doesn't exist, log a warning and insert defaults.
    pub fn report_peer(&mut self, peer_id: &PeerId, action: PeerAction, source: ReportSource) {
        // Helper function to avoid any potential deadlocks.
        let mut to_ban_peers = Vec::with_capacity(1);
        let mut to_unban_peers = Vec::with_capacity(1);

        {
            let mut peer_db = self.network_globals.peers.write();
            if let Some(info) = peer_db.peer_info_mut(peer_id) {
                let previous_state = info.score_state();
                info.apply_peer_action_to_score(action);
                metrics::inc_counter_vec(
                    &metrics::PEER_ACTION_EVENTS_PER_CLIENT,
                    &[info.client.kind.as_ref(), action.as_ref(), source.into()],
                );

                Self::handle_score_transitions(
                    previous_state,
                    peer_id,
                    info,
                    &mut to_ban_peers,
                    &mut to_unban_peers,
                    &mut self.events,
                    &self.log,
                );
                if previous_state == info.score_state() {
                    debug!(self.log, "Peer score adjusted"; "peer_id" => %peer_id, "score" => %info.score());
                }
            }
        } // end write lock

        self.ban_and_unban_peers(to_ban_peers, to_unban_peers);
    }

    /// Peers that have been returned by discovery requests that are suitable for dialing are
    /// returned here.
    ///
    /// NOTE: By dialing `PeerId`s and not multiaddrs, libp2p requests the multiaddr associated
    /// with a new `PeerId` which involves a discovery routing table lookup. We could dial the
    /// multiaddr here, however this could relate to duplicate PeerId's etc. If the lookup
    /// proves resource constraining, we should switch to multiaddr dialling here.
    #[allow(clippy::mutable_key_type)]
    pub fn peers_discovered(&mut self, results: HashMap<PeerId, Option<Instant>>) -> Vec<PeerId> {
        let mut to_dial_peers = Vec::new();

        let connected_or_dialing = self.network_globals.connected_or_dialing_peers();
        for (peer_id, min_ttl) in results {
            // There are two conditions in deciding whether to dial this peer.
            // 1. If we are less than our max connections. Discovery queries are executed to reach
            //    our target peers, so its fine to dial up to our max peers (which will get pruned
            //    in the next heartbeat down to our target).
            // 2. If the peer is one our validators require for a specific subnet, then it is
            //    considered a priority. We have pre-allocated some extra priority slots for these
            //    peers as specified by PRIORITY_PEER_EXCESS. Therefore we dial these peers, even
            //    if we are already at our max_peer limit.
            if (min_ttl.is_some() && connected_or_dialing + to_dial_peers.len() < self.max_priority_peers() || connected_or_dialing + to_dial_peers.len() < self.max_peers())
                && self.network_globals.peers.read().should_dial(&peer_id)
            {
                // This should be updated with the peer dialing. In fact created once the peer is
                // dialed
                if let Some(min_ttl) = min_ttl {
                    self.network_globals
                        .peers
                        .write()
                        .update_min_ttl(&peer_id, min_ttl);
                }
                to_dial_peers.push(peer_id);
            }
        }

        // Queue another discovery if we need to
        let peer_count = self.network_globals.connected_or_dialing_peers();
        let outbound_only_peer_count = self.network_globals.connected_outbound_only_peers();
        let min_outbound_only_target =
            (self.target_peers as f32 * MIN_OUTBOUND_ONLY_FACTOR).ceil() as usize;

        if self.discovery_enabled
            && (peer_count < self.target_peers.saturating_sub(to_dial_peers.len())
                || outbound_only_peer_count < min_outbound_only_target)
        {
            // We need more peers, re-queue a discovery lookup.
            debug!(self.log, "Starting a new peer discovery query"; "connected_peers" => peer_count, "target_peers" => self.target_peers);
            self.events.push(PeerManagerEvent::DiscoverPeers);
        }

        to_dial_peers
    }

    /// A STATUS message has been received from a peer. This resets the status timer.
    pub fn peer_statusd(&mut self, peer_id: &PeerId) {
        self.status_peers.insert(*peer_id);
    }

    /// Adds a gossipsub subscription to a peer in the peerdb.
    pub fn add_subscription(&self, peer_id: &PeerId, subnet: Subnet) {
        if let Some(info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            info.subnets.insert(subnet);
        }
    }

    /// Removes a gossipsub subscription to a peer in the peerdb.
    pub fn remove_subscription(&self, peer_id: &PeerId, subnet: Subnet) {
        if let Some(info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            info.subnets.remove(&subnet);
        }
    }

    /// Removes all gossipsub subscriptions to a peer in the peerdb.
    pub fn remove_all_subscriptions(&self, peer_id: &PeerId) {
        if let Some(info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            info.subnets = Default::default();
        }
    }

    /// Insert the sync subnet into list of long lived sync committee subnets that we need to
    /// maintain adequate number of peers for.
    pub fn add_sync_subnet(&mut self, subnet_id: SyncSubnetId, min_ttl: Instant) {
        match self.sync_committee_subnets.entry(subnet_id) {
            Entry::Vacant(_) => {
                self.sync_committee_subnets.insert(subnet_id, min_ttl);
            }
            Entry::Occupied(old) => {
                if *old.get() < min_ttl {
                    self.sync_committee_subnets.insert(subnet_id, min_ttl);
                }
            }
        }
    }

    /// The maximum number of peers we allow to connect to us. This is `target_peers` * (1 +
    /// PEER_EXCESS_FACTOR)
    fn max_peers(&self) -> usize {
        (self.target_peers as f32 * (1.0 + PEER_EXCESS_FACTOR)).ceil() as usize
    }

    /// The maximum number of peers we allow when dialing a priority peer (i.e a peer that is
    /// subscribed to subnets that our validator requires. This is `target_peers` * (1 +
    /// PEER_EXCESS_FACTOR + PRIORITY_PEER_EXCESS)
    fn max_priority_peers(&self) -> usize {
        (self.target_peers as f32 * (1.0 + PEER_EXCESS_FACTOR + PRIORITY_PEER_EXCESS)).ceil() as usize
    }

    /* Notifications from the Swarm */

    // A peer is being dialed.
    pub fn inject_dialing(&mut self, peer_id: &PeerId, enr: Option<Enr>) {
        self.inject_peer_connection(peer_id, ConnectingType::Dialing, enr);
    }

    pub fn inject_connection_established(
        &mut self,
        peer_id: PeerId,
        endpoint: ConnectedPoint,
        num_established: std::num::NonZeroU32,
        enr: Option<Enr>,
    ) {
        // Log the connection
        match &endpoint {
            ConnectedPoint::Listener { .. } => {
                debug!(self.log, "Connection established"; "peer_id" => %peer_id, "connection" => "Incoming", "connections" => %num_established);
                // Update metrics

                // Update the NAT status if there is a port in the ENR
                if self.network_globals.local_enr.read().udp().is_some() {
                    metrics::check_nat();
                }
                metrics::inc_gauge(&metrics::NETWORK_INBOUND_PEERS);
            }
            ConnectedPoint::Dialer { .. } => {
                debug!(self.log, "Connection established"; "peer_id" => %peer_id, "connection" => "Outgoing", "connections" => %num_established);
                // Update metrics
                // Update the NAT status if there is a port in the ENR
                if self.network_globals.local_enr.read().udp().is_some() {
                    metrics::check_nat();
                }
                metrics::inc_gauge(&metrics::NETWORK_OUTBOUND_PEERS);
            }
        }

        // Increment the PEERS_PER_CLIENT metric.
        // NOTE: This metric can be larger than the connected_peers client temporarily as we
        // register the connection regardless if we are about to drop the connection because of
        // banned peers or because of reaching our connection limit.
        if let Some(kind) = self
            .network_globals
            .peers
            .read()
            .peer_info(&peer_id)
            .map(|peer_info| peer_info.client.kind.clone())
        {
            metrics::inc_gauge_vec(&metrics::PEERS_PER_CLIENT, &[&kind.to_string()]);
        } else {
            metrics::inc_gauge_vec(
                &metrics::PEERS_PER_CLIENT,
                &[&self::client::ClientKind::Unknown.to_string()],
            );
        }

        // Check to make sure the peer is not supposed to be banned
        match self.ban_status(&peer_id) {
            BanResult::BadScore => {
                // This is a faulty state
                error!(self.log, "Connected to a banned peer, re-banning"; "peer_id" => %peer_id);
                // Reban the peer
                self.goodbye_peer(&peer_id, GoodbyeReason::Banned, ReportSource::PeerManager);
                return;
            }
            BanResult::BannedIp(ip_addr) => {
                // A good peer has connected to us via a banned IP address. We ban the peer and
                // prevent future connections.
                debug!(self.log, "Peer connected via banned IP. Banning"; "peer_id" => %peer_id, "banned_ip" => %ip_addr);
                self.goodbye_peer(&peer_id, GoodbyeReason::BannedIP, ReportSource::PeerManager);
                return;
            }
            BanResult::NotBanned => {}
        }

        // Check the connection limits
        if self.peer_limit_reached()
            && self
                .network_globals
                .peers
                .read()
                .peer_info(&peer_id)
                .map_or(true, |peer| !peer.has_future_duty())
        {
            // Gracefully disconnect the peer.
            self.disconnect_peer(peer_id, GoodbyeReason::TooManyPeers);
            return;
        }

        // Register the newly connected peer (regardless if we are about to disconnect them).
        // NOTE: We don't register peers that we are disconnecting immediately. The network service
        // does not need to know about these peers.
        match endpoint {
            ConnectedPoint::Listener { send_back_addr, .. } => {
                self.inject_connect_ingoing(&peer_id, send_back_addr, enr);
                if num_established == std::num::NonZeroU32::new(1).expect("valid") {
                    self.events
                        .push(PeerManagerEvent::PeerConnectedIncoming(peer_id));
                }
            }
            ConnectedPoint::Dialer { address } => {
                self.inject_connect_outgoing(&peer_id, address, enr);
                if num_established == std::num::NonZeroU32::new(1).expect("valid") {
                    self.events
                        .push(PeerManagerEvent::PeerConnectedOutgoing(peer_id));
                }
            }
        }

        // Update the metrics
        if num_established == std::num::NonZeroU32::new(1).expect("valid") {
            metrics::inc_counter(&metrics::PEER_CONNECT_EVENT_COUNT);
            metrics::set_gauge(
                &metrics::PEERS_CONNECTED,
                self.network_globals.connected_peers() as i64,
            );
        }
    }

    pub fn inject_connection_closed(
        &mut self,
        peer_id: PeerId,
        endpoint: ConnectedPoint,
        num_established: u32,
    ) {
        if num_established == 0 {
            // There are no more connections

            // Remove all subnet subscriptions from the peer_db
            self.remove_all_subscriptions(&peer_id);

            if self
                .network_globals
                .peers
                .read()
                .is_connected_or_disconnecting(&peer_id)
            {
                // We are disconnecting the peer or the peer has already been connected.
                // Both these cases, the peer has been previously registered by the peer manager and
                // potentially the application layer.
                // Inform the application.
                self.events
                    .push(PeerManagerEvent::PeerDisconnected(peer_id));
                debug!(self.log, "Peer disconnected"; "peer_id" => %peer_id);
            }

            // Decrement the PEERS_PER_CLIENT metric
            if let Some(peer_info) = self.network_globals.peers.read().peer_info(&peer_id) {
                metrics::dec_gauge_vec(
                    &metrics::PEERS_PER_CLIENT,
                    &[&peer_info.client.kind.to_string()],
                )
            }

            // NOTE: It may be the case that a rejected node, due to too many peers is disconnected
            // here and the peer manager has no knowledge of its connection. We insert it here for
            // reference so that peer manager can track this peer.
            self.inject_disconnect(&peer_id);

            // Update the prometheus metrics
            metrics::inc_counter(&metrics::PEER_DISCONNECT_EVENT_COUNT);
            metrics::set_gauge(
                &metrics::PEERS_CONNECTED,
                self.network_globals.connected_peers() as i64,
            );

            // Decrement the INBOUND/OUTBOUND peer metric
            match endpoint {
                ConnectedPoint::Listener { .. } => {
                    metrics::dec_gauge(&metrics::NETWORK_INBOUND_PEERS);
                }
                ConnectedPoint::Dialer { .. } => {
                    metrics::dec_gauge(&metrics::NETWORK_OUTBOUND_PEERS);
                }
            }
        }
    }

    /// A dial attempt has failed.
    ///
    /// NOTE: It can be the case that we are dialing a peer and during the dialing process the peer
    /// connects and the dial attempt later fails. To handle this, we only update the peer_db if
    /// the peer is not already connected.
    pub fn inject_dial_failure(&mut self, peer_id: &PeerId) {
        if !self.network_globals.peers.read().is_connected(peer_id) {
            self.inject_disconnect(peer_id);
        }
    }

    /// Reports if a peer is banned or not.
    ///
    /// This is used to determine if we should accept incoming connections.
    pub fn ban_status(&self, peer_id: &PeerId) -> BanResult {
        self.network_globals.peers.read().ban_status(peer_id)
    }

    pub fn is_connected(&self, peer_id: &PeerId) -> bool {
        self.network_globals.peers.read().is_connected(peer_id)
    }

    /// Reports whether the peer limit is reached in which case we stop allowing new incoming
    /// connections.
    pub fn peer_limit_reached(&self) -> bool {
        self.network_globals.connected_or_dialing_peers() >= self.max_peers()
    }

    /// Updates `PeerInfo` with `identify` information.
    pub fn identify(&mut self, peer_id: &PeerId, info: &IdentifyInfo) {
        if let Some(peer_info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            let previous_kind = peer_info.client.kind.clone();
            let previous_listening_addresses = std::mem::replace(
                &mut peer_info.listening_addresses,
                info.listen_addrs.clone(),
            );
            peer_info.client = client::Client::from_identify_info(info);

            if previous_kind != peer_info.client.kind
                || peer_info.listening_addresses != previous_listening_addresses
            {
                debug!(self.log, "Identified Peer"; "peer" => %peer_id,
                    "protocol_version" => &info.protocol_version,
                    "agent_version" => &info.agent_version,
                    "listening_ addresses" => ?info.listen_addrs,
                    "observed_address" => ?info.observed_addr,
                    "protocols" => ?info.protocols
                );

                // update the peer client kind metric if the peer is connected
                if matches!(
                    peer_info.connection_status(),
                    PeerConnectionStatus::Connected { .. }
                        | PeerConnectionStatus::Disconnecting { .. }
                ) {
                    metrics::inc_gauge_vec(
                        &metrics::PEERS_PER_CLIENT,
                        &[&peer_info.client.kind.to_string()],
                    );
                    metrics::dec_gauge_vec(
                        &metrics::PEERS_PER_CLIENT,
                        &[&previous_kind.to_string()],
                    );
                }
            }
        } else {
            crit!(self.log, "Received an Identify response from an unknown peer"; "peer_id" => peer_id.to_string());
        }
    }

    /// An error has occurred in the RPC.
    ///
    /// This adjusts a peer's score based on the error.
    pub fn handle_rpc_error(
        &mut self,
        peer_id: &PeerId,
        protocol: Protocol,
        err: &RPCError,
        direction: ConnectionDirection,
    ) {
        let client = self.network_globals.client(peer_id);
        let score = self.network_globals.peers.read().score(peer_id);
        debug!(self.log, "RPC Error"; "protocol" => %protocol, "err" => %err, "client" => %client,
            "peer_id" => %peer_id, "score" => %score, "direction" => ?direction);
        metrics::inc_counter_vec(
            &metrics::TOTAL_RPC_ERRORS_PER_CLIENT,
            &[
                client.kind.as_ref(),
                err.as_static_str(),
                direction.as_ref(),
            ],
        );

        // Map this error to a `PeerAction` (if any)
        let peer_action = match err {
            RPCError::IncompleteStream => {
                // They closed early, this could mean poor connection
                PeerAction::MidToleranceError
            }
            RPCError::InternalError(_) | RPCError::HandlerRejected => {
                // Our fault. Do nothing
                return;
            }
            RPCError::InvalidData => {
                // Peer is not complying with the protocol. This is considered a malicious action
                PeerAction::Fatal
            }
            RPCError::IoError(_e) => {
                // this could their fault or ours, so we tolerate this
                PeerAction::HighToleranceError
            }
            RPCError::ErrorResponse(code, _) => match code {
                RPCResponseErrorCode::Unknown => PeerAction::HighToleranceError,
                RPCResponseErrorCode::ResourceUnavailable => {
                    // NOTE: This error only makes sense for the `BlocksByRange` and `BlocksByRoot`
                    // protocols.
                    //
                    // If we are syncing, there is no point keeping these peers around and
                    // continually failing to request blocks. We instantly ban them and hope that
                    // by the time the ban lifts, the peers will have completed their backfill
                    // sync.
                    //
                    // TODO: Potentially a more graceful way of handling such peers, would be to
                    // implement a new sync type which tracks these peers and prevents the sync
                    // algorithms from requesting blocks from them (at least for a set period of
                    // time, multiple failures would then lead to a ban).
                    PeerAction::Fatal
                }
                RPCResponseErrorCode::ServerError => PeerAction::MidToleranceError,
                RPCResponseErrorCode::InvalidRequest => PeerAction::LowToleranceError,
                RPCResponseErrorCode::RateLimited => match protocol {
                    Protocol::Ping => PeerAction::MidToleranceError,
                    Protocol::BlocksByRange => PeerAction::MidToleranceError,
                    Protocol::BlocksByRoot => PeerAction::MidToleranceError,
                    Protocol::Goodbye => PeerAction::LowToleranceError,
                    Protocol::MetaData => PeerAction::LowToleranceError,
                    Protocol::Status => PeerAction::LowToleranceError,
                },
            },
            RPCError::SSZDecodeError(_) => PeerAction::Fatal,
            RPCError::UnsupportedProtocol => {
                // Not supporting a protocol shouldn't be considered a malicious action, but
                // it is an action that in some cases will make the peer unfit to continue
                // communicating.

                match protocol {
                    Protocol::Ping => PeerAction::Fatal,
                    Protocol::BlocksByRange => return,
                    Protocol::BlocksByRoot => return,
                    Protocol::Goodbye => return,
                    Protocol::MetaData => PeerAction::LowToleranceError,
                    Protocol::Status => PeerAction::LowToleranceError,
                }
            }
            RPCError::StreamTimeout => match direction {
                ConnectionDirection::Incoming => {
                    // we timed out
                    warn!(self.log, "Timed out to a peer's request. Likely too many resources, reduce peer count");
                    return;
                }
                ConnectionDirection::Outgoing => match protocol {
                    Protocol::Ping => PeerAction::LowToleranceError,
                    Protocol::BlocksByRange => PeerAction::MidToleranceError,
                    Protocol::BlocksByRoot => PeerAction::MidToleranceError,
                    Protocol::Goodbye => return,
                    Protocol::MetaData => return,
                    Protocol::Status => return,
                },
            },
            RPCError::NegotiationTimeout => PeerAction::LowToleranceError,
            RPCError::Disconnected => return, // No penalty for a graceful disconnection
        };

        self.report_peer(peer_id, peer_action, ReportSource::RPC);
    }

    /// A ping request has been received.
    // NOTE: The behaviour responds with a PONG automatically
    pub fn ping_request(&mut self, peer_id: &PeerId, seq: u64) {
        if let Some(peer_info) = self.network_globals.peers.read().peer_info(peer_id) {
            // received a ping
            // reset the to-ping timer for this peer
            debug!(self.log, "Received a ping request"; "peer_id" => %peer_id, "seq_no" => seq);
            match peer_info.connection_direction {
                Some(ConnectionDirection::Incoming) => {
                    self.inbound_ping_peers.insert(*peer_id);
                }
                Some(ConnectionDirection::Outgoing) => {
                    self.outbound_ping_peers.insert(*peer_id);
                }
                None => {
                    warn!(self.log, "Received a ping from a peer with an unknown connection direction"; "peer_id" => %peer_id);
                }
            }

            // if the sequence number is unknown send an update the meta data of the peer.
            if let Some(meta_data) = &peer_info.meta_data {
                if *meta_data.seq_number() < seq {
                    debug!(self.log, "Requesting new metadata from peer";
                        "peer_id" => %peer_id, "known_seq_no" => meta_data.seq_number(), "ping_seq_no" => seq);
                    self.events.push(PeerManagerEvent::MetaData(*peer_id));
                }
            } else {
                // if we don't know the meta-data, request it
                debug!(self.log, "Requesting first metadata from peer";
                    "peer_id" => %peer_id);
                self.events.push(PeerManagerEvent::MetaData(*peer_id));
            }
        } else {
            crit!(self.log, "Received a PING from an unknown peer";
                "peer_id" => %peer_id);
        }
    }

    /// A PONG has been returned from a peer.
    pub fn pong_response(&mut self, peer_id: &PeerId, seq: u64) {
        if let Some(peer_info) = self.network_globals.peers.read().peer_info(peer_id) {
            // received a pong

            // if the sequence number is unknown send update the meta data of the peer.
            if let Some(meta_data) = &peer_info.meta_data {
                if *meta_data.seq_number() < seq {
                    debug!(self.log, "Requesting new metadata from peer";
                        "peer_id" => %peer_id, "known_seq_no" => meta_data.seq_number(), "pong_seq_no" => seq);
                    self.events.push(PeerManagerEvent::MetaData(*peer_id));
                }
            } else {
                // if we don't know the meta-data, request it
                debug!(self.log, "Requesting first metadata from peer";
                    "peer_id" => %peer_id);
                self.events.push(PeerManagerEvent::MetaData(*peer_id));
            }
        } else {
            crit!(self.log, "Received a PONG from an unknown peer"; "peer_id" => %peer_id);
        }
    }

    /// Received a metadata response from a peer.
    pub fn meta_data_response(&mut self, peer_id: &PeerId, meta_data: MetaData<TSpec>) {
        if let Some(peer_info) = self.network_globals.peers.write().peer_info_mut(peer_id) {
            if let Some(known_meta_data) = &peer_info.meta_data {
                if *known_meta_data.seq_number() < *meta_data.seq_number() {
                    debug!(self.log, "Updating peer's metadata";
                        "peer_id" => %peer_id, "known_seq_no" => known_meta_data.seq_number(), "new_seq_no" => meta_data.seq_number());
                } else {
                    debug!(self.log, "Received old metadata";
                        "peer_id" => %peer_id, "known_seq_no" => known_meta_data.seq_number(), "new_seq_no" => meta_data.seq_number());
                    // Updating metadata even in this case to prevent storing
                    // incorrect  `attnets/syncnets` for a peer
                }
            } else {
                // we have no meta-data for this peer, update
                debug!(self.log, "Obtained peer's metadata";
                    "peer_id" => %peer_id, "new_seq_no" => meta_data.seq_number());
            }
            peer_info.meta_data = Some(meta_data);
        } else {
            crit!(self.log, "Received METADATA from an unknown peer";
                "peer_id" => %peer_id);
        }
    }

    pub(crate) fn update_gossipsub_scores(&mut self, gossipsub: &Gossipsub) {
        let mut to_ban_peers = Vec::new();
        let mut to_unban_peers = Vec::new();

        {
            // collect peers with scores
            let mut guard = self.network_globals.peers.write();
            let mut peers: Vec<_> = guard
                .peers_mut()
                .filter_map(|(peer_id, info)| {
                    gossipsub
                        .peer_score(peer_id)
                        .map(|score| (peer_id, info, score))
                })
                .collect();

            // sort descending by score
            peers.sort_unstable_by(|(.., s1), (.., s2)| {
                s2.partial_cmp(s1).unwrap_or(Ordering::Equal)
            });

            let mut to_ignore_negative_peers =
                (self.target_peers as f32 * ALLOWED_NEGATIVE_GOSSIPSUB_FACTOR).ceil() as usize;

            for (peer_id, info, score) in peers {
                let previous_state = info.score_state();
                info.update_gossipsub_score(
                    score,
                    if score < 0.0 && to_ignore_negative_peers > 0 {
                        to_ignore_negative_peers -= 1;
                        // We ignore the negative score for the best negative peers so that their
                        // gossipsub score can recover without getting disconnected.
                        true
                    } else {
                        false
                    },
                );

                Self::handle_score_transitions(
                    previous_state,
                    peer_id,
                    info,
                    &mut to_ban_peers,
                    &mut to_unban_peers,
                    &mut self.events,
                    &self.log,
                );
            }
        } // end write lock

        self.ban_and_unban_peers(to_ban_peers, to_unban_peers);
    }

    /* Internal functions */

    /// Sets a peer as connected as long as their reputation allows it
    /// Informs if the peer was accepted
    fn inject_connect_ingoing(
        &mut self,
        peer_id: &PeerId,
        multiaddr: Multiaddr,
        enr: Option<Enr>,
    ) -> bool {
        self.inject_peer_connection(peer_id, ConnectingType::IngoingConnected { multiaddr }, enr)
    }

    /// Sets a peer as connected as long as their reputation allows it
    /// Informs if the peer was accepted
    fn inject_connect_outgoing(
        &mut self,
        peer_id: &PeerId,
        multiaddr: Multiaddr,
        enr: Option<Enr>,
    ) -> bool {
        self.inject_peer_connection(
            peer_id,
            ConnectingType::OutgoingConnected { multiaddr },
            enr,
        )
    }

    /// Updates the state of the peer as disconnected.
    ///
    /// This is also called when dialing a peer fails.
    fn inject_disconnect(&mut self, peer_id: &PeerId) {
        if self
            .network_globals
            .peers
            .write()
            .inject_disconnect(peer_id)
        {
            // The peer was awaiting a ban, continue to ban the peer.
            self.ban_peer(peer_id, GoodbyeReason::BadScore);
        }

        // Remove the ping and status timer for the peer
        self.inbound_ping_peers.remove(peer_id);
        self.outbound_ping_peers.remove(peer_id);
        self.status_peers.remove(peer_id);
    }

    /// Registers a peer as connected. The `ingoing` parameter determines if the peer is being
    /// dialed or connecting to us.
    ///
    /// This is called by `connect_ingoing` and `connect_outgoing`.
    ///
    /// Informs if the peer was accepted in to the db or not.
    fn inject_peer_connection(
        &mut self,
        peer_id: &PeerId,
        connection: ConnectingType,
        enr: Option<Enr>,
    ) -> bool {
        {
            let mut peerdb = self.network_globals.peers.write();
            if !matches!(peerdb.ban_status(peer_id), BanResult::NotBanned) {
                // don't connect if the peer is banned
                slog::crit!(self.log, "Connection has been allowed to a banned peer"; "peer_id" => %peer_id);
            }

            match connection {
                ConnectingType::Dialing => {
                    peerdb.dialing_peer(peer_id, enr);
                    return true;
                }
                ConnectingType::IngoingConnected { multiaddr } => {
                    peerdb.connect_ingoing(peer_id, multiaddr, enr);
                    // start a timer to ping inbound peers.
                    self.inbound_ping_peers.insert(*peer_id);
                }
                ConnectingType::OutgoingConnected { multiaddr } => {
                    peerdb.connect_outgoing(peer_id, multiaddr, enr);
                    // start a timer for to ping outbound peers.
                    self.outbound_ping_peers.insert(*peer_id);
                }
            }
        }

        // start a ping and status timer for the peer
        self.status_peers.insert(*peer_id);

        true
    }

    /// This handles score transitions between states. It transitions peers states from
    /// disconnected/banned/connected.
    fn handle_score_transitions(
        previous_state: ScoreState,
        peer_id: &PeerId,
        info: &mut PeerInfo<TSpec>,
        to_ban_peers: &mut Vec<PeerId>,
        to_unban_peers: &mut Vec<PeerId>,
        events: &mut SmallVec<[PeerManagerEvent; 16]>,
        log: &slog::Logger,
    ) {
        match (info.score_state(), previous_state) {
            (ScoreState::Banned, ScoreState::Healthy | ScoreState::Disconnected) => {
                debug!(log, "Peer has been banned"; "peer_id" => %peer_id, "score" => %info.score());
                to_ban_peers.push(*peer_id);
            }
            (ScoreState::Disconnected, ScoreState::Banned | ScoreState::Healthy) => {
                debug!(log, "Peer transitioned to disconnect state"; "peer_id" => %peer_id, "score" => %info.score(), "past_state" => %previous_state);
                // disconnect the peer if it's currently connected or dialing
                if info.is_connected_or_dialing() {
                    // Change the state to inform that we are disconnecting the peer.
                    info.disconnecting(false);
                    events.push(PeerManagerEvent::DisconnectPeer(
                        *peer_id,
                        GoodbyeReason::BadScore,
                    ));
                } else if previous_state == ScoreState::Banned {
                    to_unban_peers.push(*peer_id);
                }
            }
            (ScoreState::Healthy, ScoreState::Disconnected) => {
                debug!(log, "Peer transitioned to healthy state"; "peer_id" => %peer_id, "score" => %info.score(), "past_state" => %previous_state);
            }
            (ScoreState::Healthy, ScoreState::Banned) => {
                debug!(log, "Peer transitioned to healthy state"; "peer_id" => %peer_id, "score" => %info.score(), "past_state" => %previous_state);
                // unban the peer if it was previously banned.
                to_unban_peers.push(*peer_id);
            }
            // Explicitly ignore states that haven't transitioned.
            (ScoreState::Healthy, ScoreState::Healthy) => {}
            (ScoreState::Disconnected, ScoreState::Disconnected) => {}
            (ScoreState::Banned, ScoreState::Banned) => {}
        }
    }

    /// Updates the state of banned peers.
    fn ban_and_unban_peers(&mut self, to_ban_peers: Vec<PeerId>, to_unban_peers: Vec<PeerId>) {
        // process banning peers
        for peer_id in to_ban_peers {
            self.ban_peer(&peer_id, GoodbyeReason::BadScore);
        }
        // process unbanning peers
        for peer_id in to_unban_peers {
            if let Err(e) = self.unban_peer(&peer_id) {
                error!(self.log, "{}", e; "peer_id" => %peer_id);
            }
        }
    }

    /// Updates the scores of known peers according to their connection
    /// status and the time that has passed.
    /// NOTE: This is experimental and will likely be adjusted
    fn update_peer_scores(&mut self) {
        /* Check how long have peers been in this state and update their reputations if needed */
        let mut to_ban_peers = Vec::new();
        let mut to_unban_peers = Vec::new();

        for (peer_id, info) in self.network_globals.peers.write().peers_mut() {
            let previous_state = info.score_state();
            // Update scores
            info.score_update();

            Self::handle_score_transitions(
                previous_state,
                peer_id,
                info,
                &mut to_ban_peers,
                &mut to_unban_peers,
                &mut self.events,
                &self.log,
            );
        }
        self.ban_and_unban_peers(to_ban_peers, to_unban_peers);
    }

    /// Bans a peer.
    ///
    /// Records updates the peers connection status and updates the peer db as well as blocks the
    /// peer from participating in discovery and removes them from the routing table.
    fn ban_peer(&mut self, peer_id: &PeerId, reason: GoodbyeReason) {
        // NOTE: When we ban a peer, its IP address can be banned. We do not recursively search
        // through all our connected peers banning all other peers that are using this IP address.
        // If these peers are behaving fine, we permit their current connections. However, if any new
        // nodes or current nodes try to reconnect on a banned IP, they will be instantly banned
        // and disconnected.

        let mut peer_db = self.network_globals.peers.write();

        match peer_db.disconnect_and_ban(peer_id) {
            BanOperation::DisconnectThePeer => {
                // The peer was currently connected, so we start a disconnection.
                // Once the peer has disconnected, this function will be called to again to ban
                // at the swarm level.
                self.events
                    .push(PeerManagerEvent::DisconnectPeer(*peer_id, reason));
            }
            BanOperation::PeerDisconnecting => {
                // The peer is currently being disconnected and will be banned once the
                // disconnection completes.
            }
            BanOperation::ReadyToBan => {
                // The peer is not currently connected, we can safely ban it at the swarm
                // level.
                let banned_ip_addresses = peer_db
                    .peer_info(peer_id)
                    .map(|info| {
                        info.seen_addresses()
                            .filter(|ip| peer_db.is_ip_banned(ip))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                // Inform the Swarm to ban the peer
                self.events
                    .push(PeerManagerEvent::Banned(*peer_id, banned_ip_addresses));
            }
        }
    }

    /// Unbans a peer.
    ///
    /// Updates the peer's connection status and updates the peer db as well as removes
    /// previous bans from discovery.
    fn unban_peer(&mut self, peer_id: &PeerId) -> Result<(), &'static str> {
        let mut peer_db = self.network_globals.peers.write();
        peer_db.unban(peer_id)?;

        let seen_ip_addresses = peer_db
            .peer_info(peer_id)
            .map(|info| info.seen_addresses().collect::<Vec<_>>())
            .unwrap_or_default();

        // Inform the Swarm to unban the peer
        self.events
            .push(PeerManagerEvent::UnBanned(*peer_id, seen_ip_addresses));
        Ok(())
    }

    // Gracefully disconnects a peer without banning them.
    fn disconnect_peer(&mut self, peer_id: PeerId, reason: GoodbyeReason) {
        self.events
            .push(PeerManagerEvent::DisconnectPeer(peer_id, reason));
        self.network_globals
            .peers
            .write()
            .notify_disconnecting(peer_id, false);
    }

    /// Run discovery query for additional sync committee peers if we fall below `TARGET_PEERS`.
    fn maintain_sync_committee_peers(&mut self) {
        // Remove expired entries
        self.sync_committee_subnets
            .retain(|_, v| *v > Instant::now());

        let subnets_to_discover: Vec<SubnetDiscovery> = self
            .sync_committee_subnets
            .iter()
            .filter_map(|(k, v)| {
                if self
                    .network_globals
                    .peers
                    .read()
                    .good_peers_on_subnet(Subnet::SyncCommittee(*k))
                    .count()
                    < TARGET_SUBNET_PEERS
                {
                    Some(SubnetDiscovery {
                        subnet: Subnet::SyncCommittee(*k),
                        min_ttl: Some(*v),
                    })
                } else {
                    None
                }
            })
            .collect();

        // request the subnet query from discovery
        if !subnets_to_discover.is_empty() {
            debug!(
                self.log,
                "Making subnet queries for maintaining sync committee peers";
                "subnets" => ?subnets_to_discover.iter().map(|s| s.subnet).collect::<Vec<_>>()
            );
            self.events
                .push(PeerManagerEvent::DiscoverSubnetPeers(subnets_to_discover));
        }
    }

    /// The Peer manager's heartbeat maintains the peer count and maintains peer reputations.
    ///
    /// It will request discovery queries if the peer count has not reached the desired number of
    /// overall peers, as well as the desired number of outbound-only peers.
    ///
    /// NOTE: Discovery will only add a new query if one isn't already queued.
    fn heartbeat(&mut self) {
        let peer_count = self.network_globals.connected_or_dialing_peers();
        let mut outbound_only_peer_count = self.network_globals.connected_outbound_only_peers();
        let min_outbound_only_target =
            (self.target_peers as f32 * MIN_OUTBOUND_ONLY_FACTOR).ceil() as usize;

        if self.discovery_enabled
            && (peer_count < self.target_peers
                || outbound_only_peer_count < min_outbound_only_target)
        {
            // If we need more peers, queue a discovery lookup.
            debug!(self.log, "Starting a new peer discovery query"; "connected_peers" => peer_count, "target_peers" => self.target_peers);
            self.events.push(PeerManagerEvent::DiscoverPeers);
        }

        // Updates peer's scores.
        self.update_peer_scores();

        // Update peer score metrics;
        self.update_peer_score_metrics();

        // Maintain minimum count for sync committee peers.
        self.maintain_sync_committee_peers();

        // Keep a list of peers we are disconnecting
        let mut disconnecting_peers = Vec::new();

        let connected_peer_count = self.network_globals.connected_peers();
        if connected_peer_count > self.target_peers {
            // Remove excess peers with the worst scores, but keep subnet peers.
            // Must also ensure that the outbound-only peer count does not go below the minimum threshold.
            outbound_only_peer_count = self.network_globals.connected_outbound_only_peers();
            let mut n_outbound_removed = 0;
            for (peer_id, info) in self
                .network_globals
                .peers
                .read()
                .worst_connected_peers()
                .iter()
                .filter(|(_, info)| !info.has_future_duty())
            {
                if disconnecting_peers.len() == connected_peer_count - self.target_peers {
                    break;
                }
                if info.is_outbound_only() {
                    if min_outbound_only_target < outbound_only_peer_count - n_outbound_removed {
                        n_outbound_removed += 1;
                    } else {
                        continue;
                    }
                }
                disconnecting_peers.push(**peer_id);
            }
        }

        for peer_id in disconnecting_peers {
            self.disconnect_peer(peer_id, GoodbyeReason::TooManyPeers);
        }
    }

    // Update metrics related to peer scoring.
    fn update_peer_score_metrics(&self) {
        // reset the gauges
        let _ = metrics::PEER_SCORE_DISTRIBUTION
            .as_ref()
            .map(|gauge| gauge.reset());
        let _ = metrics::PEER_SCORE_PER_CLIENT
            .as_ref()
            .map(|gauge| gauge.reset());

        let mut avg_score_per_client: HashMap<String, (f64, usize)> = HashMap::with_capacity(5);
        {
            let peers_db_read_lock = self.network_globals.peers.read();
            let connected_peers = peers_db_read_lock.best_peers_by_status(PeerInfo::is_connected);
            let total_peers = connected_peers.len();
            for (id, (_peer, peer_info)) in connected_peers.into_iter().enumerate() {
                // First quartile
                if id == 0 {
                    metrics::set_gauge_vec(
                        &metrics::PEER_SCORE_DISTRIBUTION,
                        &["1st"],
                        peer_info.score().score() as i64,
                    );
                } else if id == (total_peers * 3 / 4).saturating_sub(1) {
                    metrics::set_gauge_vec(
                        &metrics::PEER_SCORE_DISTRIBUTION,
                        &["3/4"],
                        peer_info.score().score() as i64,
                    );
                } else if id == (total_peers / 2).saturating_sub(1) {
                    metrics::set_gauge_vec(
                        &metrics::PEER_SCORE_DISTRIBUTION,
                        &["1/2"],
                        peer_info.score().score() as i64,
                    );
                } else if id == (total_peers / 4).saturating_sub(1) {
                    metrics::set_gauge_vec(
                        &metrics::PEER_SCORE_DISTRIBUTION,
                        &["1/4"],
                        peer_info.score().score() as i64,
                    );
                } else if id == total_peers.saturating_sub(1) {
                    metrics::set_gauge_vec(
                        &metrics::PEER_SCORE_DISTRIBUTION,
                        &["last"],
                        peer_info.score().score() as i64,
                    );
                }

                let mut score_peers: &mut (f64, usize) = avg_score_per_client
                    .entry(peer_info.client.kind.to_string())
                    .or_default();
                score_peers.0 += peer_info.score().score();
                score_peers.1 += 1;
            }
        } // read lock ended

        for (client, (score, peers)) in avg_score_per_client {
            metrics::set_float_gauge_vec(
                &metrics::PEER_SCORE_PER_CLIENT,
                &[&client.to_string()],
                score / (peers as f64),
            );
        }
    }
}

impl<TSpec: EthSpec> Stream for PeerManager<TSpec> {
    type Item = PeerManagerEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // perform the heartbeat when necessary
        while self.heartbeat.poll_tick(cx).is_ready() {
            self.heartbeat();
        }

        // poll the timeouts for pings and status'
        loop {
            match self.inbound_ping_peers.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(peer_id))) => {
                    self.inbound_ping_peers.insert(peer_id);
                    self.events.push(PeerManagerEvent::Ping(peer_id));
                }
                Poll::Ready(Some(Err(e))) => {
                    error!(self.log, "Failed to check for inbound peers to ping"; "error" => e.to_string())
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }

        loop {
            match self.outbound_ping_peers.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(peer_id))) => {
                    self.outbound_ping_peers.insert(peer_id);
                    self.events.push(PeerManagerEvent::Ping(peer_id));
                }
                Poll::Ready(Some(Err(e))) => {
                    error!(self.log, "Failed to check for outbound peers to ping"; "error" => e.to_string())
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }

        if !matches!(
            self.network_globals.sync_state(),
            SyncState::SyncingFinalized { .. } | SyncState::SyncingHead { .. }
        ) {
            loop {
                match self.status_peers.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(peer_id))) => {
                        self.status_peers.insert(peer_id);
                        self.events.push(PeerManagerEvent::Status(peer_id))
                    }
                    Poll::Ready(Some(Err(e))) => {
                        error!(self.log, "Failed to check for peers to ping"; "error" => e.to_string())
                    }
                    Poll::Ready(None) | Poll::Pending => break,
                }
            }
        }

        if !self.events.is_empty() {
            return Poll::Ready(Some(self.events.remove(0)));
        } else {
            self.events.shrink_to_fit();
        }

        Poll::Pending
    }
}

enum ConnectingType {
    /// We are in the process of dialing this peer.
    Dialing,
    /// A peer has dialed us.
    IngoingConnected {
        // The multiaddr the peer connected to us on.
        multiaddr: Multiaddr,
    },
    /// We have successfully dialed a peer.
    OutgoingConnected {
        /// The multiaddr we dialed to reach the peer.
        multiaddr: Multiaddr,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::enr::build_enr;
    use crate::discovery::enr_ext::CombinedKeyExt;
    use crate::rpc::methods::{MetaData, MetaDataV2};
    use crate::Enr;
    use discv5::enr::CombinedKey;
    use slog::{o, Drain};
    use std::net::UdpSocket;
    use types::{EnrForkId, MinimalEthSpec};

    type E = MinimalEthSpec;

    pub fn unused_port() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("should create udp socket");
        let local_addr = socket.local_addr().expect("should read udp socket");
        local_addr.port()
    }

    pub fn build_log(level: slog::Level, enabled: bool) -> slog::Logger {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();

        if enabled {
            slog::Logger::root(drain.filter_level(level).fuse(), o!())
        } else {
            slog::Logger::root(drain.filter(|_| false).fuse(), o!())
        }
    }

    async fn build_peer_manager(target: usize) -> PeerManager<E> {
        let keypair = libp2p::identity::Keypair::generate_secp256k1();
        let config = NetworkConfig {
            discovery_port: unused_port(),
            target_peers: target,
            ..Default::default()
        };
        let enr_key: CombinedKey = CombinedKey::from_libp2p(&keypair).unwrap();
        let enr: Enr = build_enr::<E>(&enr_key, &config, EnrForkId::default()).unwrap();
        let log = build_log(slog::Level::Debug, false);
        let globals = NetworkGlobals::new(
            enr,
            9000,
            9000,
            MetaData::V2(MetaDataV2 {
                seq_number: 0,
                attnets: Default::default(),
                syncnets: Default::default(),
            }),
            vec![],
            &log,
        );
        PeerManager::new(&config, Arc::new(globals), &log)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_peer_manager_disconnects_correctly_during_heartbeat() {
        let mut peer_manager = build_peer_manager(3).await;

        // Create 5 peers to connect to.
        // 2 will be outbound-only, and have the lowest score.
        let peer0 = PeerId::random();
        let peer1 = PeerId::random();
        let peer2 = PeerId::random();
        let outbound_only_peer1 = PeerId::random();
        let outbound_only_peer2 = PeerId::random();

        peer_manager.inject_connect_ingoing(&peer0, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_ingoing(&peer1, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_ingoing(&peer2, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_outgoing(
            &outbound_only_peer1,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        peer_manager.inject_connect_outgoing(
            &outbound_only_peer2,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );

        // Set the outbound-only peers to have the lowest score.
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&outbound_only_peer1)
            .unwrap()
            .add_to_score(-1.0);

        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&outbound_only_peer2)
            .unwrap()
            .add_to_score(-2.0);

        // Check initial connected peers.
        assert_eq!(peer_manager.network_globals.connected_or_dialing_peers(), 5);

        peer_manager.heartbeat();

        // Check that we disconnected from two peers.
        // Check that one outbound-only peer was removed because it had the worst score
        // and that we did not disconnect the other outbound peer due to the minimum outbound quota.
        assert_eq!(peer_manager.network_globals.connected_or_dialing_peers(), 3);
        assert!(peer_manager
            .network_globals
            .peers
            .read()
            .is_connected(&outbound_only_peer1));
        assert!(!peer_manager
            .network_globals
            .peers
            .read()
            .is_connected(&outbound_only_peer2));

        peer_manager.heartbeat();

        // Check that if we are at target number of peers, we do not disconnect any.
        assert_eq!(peer_manager.network_globals.connected_or_dialing_peers(), 3);
    }

    #[tokio::test]
    async fn test_peer_manager_not_enough_outbound_peers_no_panic_during_heartbeat() {
        let mut peer_manager = build_peer_manager(20).await;

        // Connect to 20 ingoing-only peers.
        for _i in 0..19 {
            let peer = PeerId::random();
            peer_manager.inject_connect_ingoing(&peer, "/ip4/0.0.0.0".parse().unwrap(), None);
        }

        // Connect an outbound-only peer.
        // Give it the lowest score so that it is evaluated first in the disconnect list iterator.
        let outbound_only_peer = PeerId::random();
        peer_manager.inject_connect_ingoing(
            &outbound_only_peer,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(outbound_only_peer))
            .unwrap()
            .add_to_score(-1.0);
        // After heartbeat, we will have removed one peer.
        // Having less outbound-only peers than minimum won't cause panic when the outbound-only peer is being considered for disconnection.
        peer_manager.heartbeat();
        assert_eq!(
            peer_manager.network_globals.connected_or_dialing_peers(),
            20
        );
    }

    #[tokio::test]
    async fn test_peer_manager_removes_unhealthy_peers_during_heartbeat() {
        let mut peer_manager = build_peer_manager(3).await;

        // Create 3 peers to connect to.
        let peer0 = PeerId::random();
        let inbound_only_peer1 = PeerId::random();
        let outbound_only_peer1 = PeerId::random();

        peer_manager.inject_connect_ingoing(&peer0, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_outgoing(&peer0, "/ip4/0.0.0.0".parse().unwrap(), None);

        // Connect to two peers that are on the threshold of being disconnected.
        peer_manager.inject_connect_ingoing(
            &inbound_only_peer1,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        peer_manager.inject_connect_outgoing(
            &outbound_only_peer1,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(inbound_only_peer1))
            .unwrap()
            .add_to_score(-19.9);
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(outbound_only_peer1))
            .unwrap()
            .add_to_score(-19.9);
        // Update the gossipsub scores to induce connection downgrade
        // during the heartbeat, update_peer_scores will downgrade the score from -19.9 to at least -20, this will then trigger a disconnection.
        // If we changed the peer scores to -20 before the heartbeat, update_peer_scores will mark the previous score status as disconnected,
        // then handle_state_transitions will not change the connection status to disconnected because the score state has not changed.
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(inbound_only_peer1))
            .unwrap()
            .set_gossipsub_score(-85.0);
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(outbound_only_peer1))
            .unwrap()
            .set_gossipsub_score(-85.0);

        peer_manager.heartbeat();

        assert_eq!(peer_manager.network_globals.connected_or_dialing_peers(), 1);
    }

    #[tokio::test]
    async fn test_peer_manager_remove_unhealthy_peers_brings_peers_below_target() {
        let mut peer_manager = build_peer_manager(3).await;

        // Create 4 peers to connect to.
        // One pair will be unhealthy inbound only and outbound only peers.
        let peer0 = PeerId::random();
        let peer1 = PeerId::random();
        let inbound_only_peer1 = PeerId::random();
        let outbound_only_peer1 = PeerId::random();

        peer_manager.inject_connect_ingoing(&peer0, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_ingoing(&peer1, "/ip4/0.0.0.0".parse().unwrap(), None);

        // Connect to two peers that are on the threshold of being disconnected.
        peer_manager.inject_connect_ingoing(
            &inbound_only_peer1,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        peer_manager.inject_connect_outgoing(
            &outbound_only_peer1,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(inbound_only_peer1))
            .unwrap()
            .add_to_score(-19.9);
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(outbound_only_peer1))
            .unwrap()
            .add_to_score(-19.9);
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(inbound_only_peer1))
            .unwrap()
            .set_gossipsub_score(-85.0);
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(outbound_only_peer1))
            .unwrap()
            .set_gossipsub_score(-85.0);
        peer_manager.heartbeat();
        // Tests that when we are over the target peer limit, after disconnecting two unhealthy peers,
        // the loop to check for disconnecting peers will stop because we have removed enough peers (only needed to remove 1 to reach target).
        assert_eq!(peer_manager.network_globals.connected_or_dialing_peers(), 2);
    }

    #[tokio::test]
    async fn test_peer_manager_removes_enough_peers_when_one_is_unhealthy() {
        let mut peer_manager = build_peer_manager(3).await;

        // Create 5 peers to connect to.
        // One will be unhealthy inbound only and outbound only peers.
        let peer0 = PeerId::random();
        let peer1 = PeerId::random();
        let peer2 = PeerId::random();
        let inbound_only_peer1 = PeerId::random();
        let outbound_only_peer1 = PeerId::random();

        peer_manager.inject_connect_ingoing(&peer0, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_ingoing(&peer1, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_ingoing(&peer2, "/ip4/0.0.0.0".parse().unwrap(), None);
        peer_manager.inject_connect_outgoing(
            &outbound_only_peer1,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        // Have one peer be on the verge of disconnection.
        peer_manager.inject_connect_ingoing(
            &inbound_only_peer1,
            "/ip4/0.0.0.0".parse().unwrap(),
            None,
        );
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(inbound_only_peer1))
            .unwrap()
            .add_to_score(-19.9);
        peer_manager
            .network_globals
            .peers
            .write()
            .peer_info_mut(&(inbound_only_peer1))
            .unwrap()
            .set_gossipsub_score(-85.0);

        peer_manager.heartbeat();
        // Tests that when we are over the target peer limit, after disconnecting an unhealthy peer,
        // the number of connected peers updates and we will not remove too many peers.
        assert_eq!(peer_manager.network_globals.connected_or_dialing_peers(), 3);
    }
}

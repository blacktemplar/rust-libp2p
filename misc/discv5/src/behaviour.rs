use self::query_info::{QueryInfo, QueryType};
use crate::kbucket::{self, KBucketsTable, NodeStatus};
use crate::packet::NodeId;
use crate::query::{Query, QueryConfig, QueryState, ReturnPeer};
use crate::rpc;
use crate::session_service::{SessionEvent, SessionService};
use enr::Enr;
use fnv::{FnvHashMap, FnvHashSet};
use futures::prelude::*;
use libp2p_core::identity::Keypair;
use libp2p_core::swarm::{
    ConnectedPoint, NetworkBehaviour, NetworkBehaviourAction, PollParameters,
};
use libp2p_core::{
    multiaddr::{Multiaddr, Protocol},
    protocols_handler::{DummyProtocolsHandler, ProtocolsHandler},
    PeerId,
};
use log::{debug, error, warn};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::io;
use std::{marker::PhantomData, time::Duration};
use tokio_io::{AsyncRead, AsyncWrite};

mod query_info;

type QueryId = usize;
type RpcId = u64;

#[derive(Clone, PartialEq, Eq, Hash)]
struct RpcRequest(RpcId, NodeId);

pub struct Discv5<TSubstream> {
    /// The local ENR for this node.
    local_enr: Enr,

    /// The keypair for the current node. Required to sign our local ENR when our address is
    /// updated.
    keypair: Keypair,

    /// Events yielded by this behaviour.
    events: SmallVec<[Discv5Event; 32]>,

    /// Abstract the NodeId from libp2p. For all known ENR's we keep a mapping of PeerId to NodeId.
    known_peer_ids: HashMap<PeerId, NodeId>,

    /// Storage of the ENR record for each node.
    kbuckets: KBucketsTable<NodeId, Enr>,

    /// All the iterative queries we are currently performing, with their ID. The last parameter
    /// is the list of accumulated providers for `GET_PROVIDERS` queries.
    active_queries: FnvHashMap<QueryId, Query<QueryInfo, NodeId>>,

    /// RPC requests that have been sent and are awaiting a response.
    active_rpc_requests: FnvHashMap<RpcRequest, QueryId>,

    /// List of peers we have established sessions with.
    connected_peers: FnvHashSet<NodeId>,

    /// The configuration for iterative queries.
    query_config: QueryConfig,

    /// Identifier for the next query that we start.
    next_query_id: QueryId,

    /// Main discv5 UDP service that establishes sessions with peers.
    service: SessionService,

    /// Marker to pin the generics.
    marker: PhantomData<TSubstream>,
}

impl<TSubstream> Discv5<TSubstream> {
    /// Builds the `Discv5` main struct.
    ///
    /// `local_enr` is the `ENR` representing the local node. This contains node identifying information, such
    /// as IP addresses and ports which we wish to broadcast to other nodes via this discovery
    /// mechanism. The `ip` and `port` fields of the ENR will determine the ip/port that the discv5
    /// `Service` will listen on.
    pub fn new(local_enr: Enr, keypair: Keypair) -> io::Result<Self> {
        let service = SessionService::new(local_enr.clone(), keypair.clone())?;
        let query_config = QueryConfig::default();

        Ok(Discv5 {
            local_enr: local_enr.clone(),
            keypair,
            events: SmallVec::new(),
            known_peer_ids: HashMap::new(),
            kbuckets: KBucketsTable::new(local_enr.node_id.into(), Duration::from_secs(60)),
            active_queries: Default::default(),
            active_rpc_requests: Default::default(),
            connected_peers: Default::default(),
            next_query_id: 0,
            query_config,
            service,
            marker: PhantomData,
        })
    }

    /// Adds a known ENR of a peer participating in Discv5 to the
    /// routing table.
    ///
    /// This allows pre-populating the Kademlia routing table with known
    /// addresses, so that they can be used immediately in following DHT
    /// operations involving one of these peers, without having to dial
    /// them upfront.
    pub fn add_enr(&mut self, enr: Enr) {
        // add to the known_peer_ids mapping
        self.known_peer_ids
            .insert(enr.peer_id().clone(), enr.node_id.clone());
        let key = kbucket::Key::from(enr.clone().node_id);
        match self.kbuckets.entry(&key) {
            kbucket::Entry::Present(mut entry, _) => {
                *entry.value() = enr;
            }
            kbucket::Entry::Pending(mut entry, _) => {
                *entry.value() = enr;
            }
            kbucket::Entry::Absent(entry) => {
                match entry.insert(enr.clone(), NodeStatus::Disconnected) {
                    kbucket::InsertResult::Inserted => {
                        let event = Discv5Event::EnrAdded {
                            enr,
                            replaced: None,
                        };
                        self.events.push(event);
                    }
                    kbucket::InsertResult::Full => (),
                    kbucket::InsertResult::Pending { disconnected } => {
                        // TODO: Establish connection.  Look up known addresses. Call PING
                    }
                }
                return;
            }
            kbucket::Entry::SelfEntry => return,
        };
    }

    /// Returns an iterator over all ENR node IDs of nodes currently contained in a bucket
    /// of the Kademlia routing table.
    pub fn kbuckets_entries(&mut self) -> impl Iterator<Item = &NodeId> {
        self.kbuckets.iter().map(|entry| entry.node.key.preimage())
    }

    /// Starts an iterative `FIND_NODE` request.
    ///
    /// This will eventually produce an event containing the nodes of the DHT closest to the
    /// requested `PeerId`.
    pub fn find_node(&mut self, node_id: NodeId) {
        self.start_query(QueryType::FindNode(node_id));
    }

    /// Constructs and sends an RPC to the session service given a `QueryInfo`.
    fn send_rpc(
        &mut self,
        query_id: &QueryId,
        query_info: &QueryInfo,
        return_peer: &ReturnPeer<NodeId>,
    ) {
        let node_id = return_peer.node_id;

        // find the destination ENR
        let dst_enr = match self.find_enr(&node_id) {
            Some(enr) => enr,
            None =>
            // search the untrusted ENR list
            {
                query_info
                    .untrusted_enrs
                    .iter()
                    .find(|e| e.node_id == node_id)
                    .expect("Send_RPC should only be called by known nodes. ENR must exist")
                    .clone()
            }
        };

        // Generate a random rpc_id which is matched per node id
        let id: u64 = rand::random();
        let req = RpcRequest(id.clone(), dst_enr.node_id);

        let body = match query_info.into_rpc_request(return_peer) {
            Ok(r) => r,
            Err(e) => {
                //dst node is local_key, report failure
                error!("Send RPC: {}", e);
                if let Some(query) = self.active_queries.get_mut(&query_id) {
                    query.on_failure(&node_id);
                }
                return;
            }
        };

        self.active_rpc_requests
            .insert(req.clone(), query_id.clone());
        match self
            .service
            .send_message(&dst_enr, rpc::ProtocolMessage { id, body })
        {
            Ok(_) => {}
            Err(_) => {
                self.active_rpc_requests.remove(&req);
                if let Some(query) = self.active_queries.get_mut(&query_id) {
                    query.on_failure(&node_id);
                }
            }
        }
    }

    /// Internal function that starts a query.
    fn start_query(&mut self, query_type: QueryType) {
        let query_id = self.next_query_id;
        self.next_query_id += 1;

        let target = QueryInfo {
            query_type: query_type,
            untrusted_enrs: Default::default(),
        };

        // How many times to call the rpc per node.
        // FINDNODE requires multiple iterations
        let query_iterations = target.iterations();

        let target_key: kbucket::Key<QueryInfo> = target.clone().into();

        let known_closest_peers = self.kbuckets.closest_keys(&target_key);
        let query = Query::with_config(
            self.query_config.clone(),
            target,
            known_closest_peers,
            query_iterations,
        );

        self.active_queries.insert(query_id, query);
    }

    /// Processes discovered peers from a query.
    fn discovered<'a, I>(&mut self, query_id: &QueryId, source: &NodeId, peers: I)
    where
        I: Iterator<Item = &'a DiscoveredPeer> + Clone,
    {
        let local_id = self.kbuckets.local_key().preimage().clone();
        let others_iter = peers.filter(|p| p.enr.node_id != local_id);

        for peer in others_iter.clone() {
            self.events.push(Discv5Event::Discovered {
                enr_id: peer.enr.node_id.clone(),
                addresses: peer.enr.multiaddr().clone(),
                ty: peer.connection_type.clone(),
            });
        }

        if let Some(query) = self.active_queries.get_mut(query_id) {
            for peer in others_iter.clone() {
                if query
                    .target_mut()
                    .untrusted_enrs
                    .iter()
                    .position(|e| e.node_id == peer.enr.node_id)
                    .is_none()
                {
                    query.target_mut().untrusted_enrs.push(peer.enr.clone());
                }
            }
            query.on_success(source, others_iter.cloned().map(|kp| kp.enr.node_id))
        }
    }

    /// Returns nodes in a specific k-bucket defined by the distance parameter. This is run in the
    /// context of a `source` peer which is not included in the result.
    fn get_nodes_by_distance<T: Clone>(
        &mut self,
        target: &kbucket::Key<T>,
        distance: u64,
        source: &NodeId,
    ) -> Vec<DiscoveredPeer> {
        let local_key = self.kbuckets.local_key().clone();

        self.kbuckets
            .closest(target)
            .filter(|e| e.node.key.preimage() != source)
            .take_while(|e| local_key.log2_distance(&e.node.key) == Some(distance))
            .map(DiscoveredPeer::from)
            .collect()
    }

    /// Update the connection status of a peer in the Kademlia routing table.
    fn connection_updated(&mut self, node_id: NodeId, enr: Option<Enr>, new_status: NodeStatus) {
        let key = kbucket::Key::new(node_id.clone());
        // add the known PeerId
        if let Some(enr_copy) = enr.clone() {
            self.known_peer_ids
                .insert(enr_copy.peer_id(), enr_copy.node_id);
        }
        match self.kbuckets.entry(&key) {
            kbucket::Entry::Present(mut entry, old_status) => {
                if let Some(enr) = enr {
                    *entry.value() = enr;
                }
                if old_status != new_status {
                    entry.update(new_status);
                }
            }

            kbucket::Entry::Pending(mut entry, old_status) => {
                if let Some(enr) = enr {
                    *entry.value() = enr;
                }
                if old_status != new_status {
                    entry.update(new_status);
                }
            }

            kbucket::Entry::Absent(entry) => {
                if new_status == NodeStatus::Connected {
                    // Note: If an ENR is not provided, no record is added
                    debug_assert!(enr.is_some());
                    if let Some(enr) = enr {
                        match entry.insert(enr, new_status) {
                            kbucket::InsertResult::Inserted => {
                                let event = Discv5Event::NodeInserted {
                                    node_id: node_id.clone(),
                                    replaced: None,
                                };
                                self.events.push(event);
                            }
                            kbucket::InsertResult::Full => (),
                            kbucket::InsertResult::Pending { disconnected } => {
                                debug_assert!(!self
                                    .connected_peers
                                    .contains(disconnected.preimage()));

                                // TODO: Connect to peer, disconnected.into_preimage().
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The equivalent of libp2p `inject_connected()` for a udp session. We have no stream, but a
    /// session key-pair has been negotiated.
    fn inject_session_established(&mut self, node_id: NodeId, enr: Enr) {
        self.known_peer_ids.insert(enr.peer_id(), node_id.clone());
        self.connection_updated(node_id, Some(enr), NodeStatus::Connected);
        self.connected_peers.insert(node_id);
    }

    /// A session could not be established for the given `NodeId`.
    fn session_not_established(&mut self, node_id: NodeId) {
        // remove the node from untrusted addresses
        for query in self.active_queries.values_mut() {
            query
                .target_mut()
                .untrusted_enrs
                .retain(|e| e.node_id != node_id);
        }

        // a seperate call for RPC failure will occur to handle active queries awaiting a
        // response
        self.connection_updated(node_id.clone(), None, NodeStatus::Disconnected);
    }

    /// A session could not be established or an RPC request timed-out (after a few retries).
    fn rpc_failure(&mut self, node_id: NodeId, failed_rpc_id: RpcId) {
        let req = RpcRequest(failed_rpc_id, node_id);

        if let Some(query_id) = self.active_rpc_requests.get(&req) {
            if let Some(query) = self.active_queries.get_mut(&query_id) {
                query.on_failure(&node_id);
            }
        }

        // report the node as being disconnected.
        self.connection_updated(node_id.clone(), None, NodeStatus::Disconnected);
        self.connected_peers.remove(&node_id);
    }

    fn find_enr(&mut self, node_id: &NodeId) -> Option<Enr> {
        // check if we know this node id in our routing table
        let key = kbucket::Key::new(node_id.clone());
        if let kbucket::Entry::Present(mut entry, _) = self.kbuckets.entry(&key) {
            return Some(entry.value().clone());
        }
        None
    }

    // TODO: split up functionality
    fn incomming_rpc(&mut self, msg: rpc::ProtocolMessage) {
        match msg.body {
            rpc::RpcType::Request(req) => {
                match req {
                    rpc::Request::FindNode { distance } => {
                        //self.get_nodes_by_distance(
                    }
                    _ => {} //TODO: Implement all RPC methods
                }
            }
            rpc::RpcType::Response(res) => {
                // verify we know of the rpc_id
            }
        }
    }
}

/// Wrapper around a discovered peer that associates a connection type to peer's ENR.
#[derive(Debug, Clone)]
struct DiscoveredPeer {
    enr: Enr,
    connection_type: ConnectionType,
}

/// The connection type associated with a discovered peer.
#[derive(Debug, Clone)]
pub enum ConnectionType {
    /// A session has been established with the peer.
    Connected,
    /// An attempt to establish a session failed.
    CouldNotConnect,
    /// No session has been attempted.
    NotConnected,
}

impl From<kbucket::EntryView<NodeId, Enr>> for DiscoveredPeer {
    fn from(e: kbucket::EntryView<NodeId, Enr>) -> DiscoveredPeer {
        DiscoveredPeer {
            enr: e.node.value,
            connection_type: match e.status {
                NodeStatus::Connected => ConnectionType::Connected,
                NodeStatus::Disconnected => ConnectionType::NotConnected,
            },
        }
    }
}

impl<TSubstream> NetworkBehaviour for Discv5<TSubstream>
where
    TSubstream: AsyncRead + AsyncWrite,
{
    type ProtocolsHandler = DummyProtocolsHandler<TSubstream>;
    type OutEvent = Discv5Event;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        DummyProtocolsHandler::default()
    }

    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        // Addresses are ordered by decreasing likelyhood of connectivity, so start with
        // the addresses of that peer in the k-buckets.

        if let Some(node_id) = self.known_peer_ids.get(peer_id) {
            let key = kbucket::Key::new(node_id.clone());
            let mut out_list =
                if let kbucket::Entry::Present(mut entry, _) = self.kbuckets.entry(&key) {
                    entry
                        .value()
                        .multiaddr()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };

            // ENR's may have multiple Multiaddrs. The multi-addr associated with the UDP
            // port is removed, which is assumed to be associated with the discv5 protocol (and
            // therefore irrelevant for other libp2p components).
            out_list.retain(|addr| {
                addr.iter()
                    .find(|v| match v {
                        Protocol::Udp(_) => true,
                        _ => false,
                    })
                    .is_none()
            });

            out_list
        } else {
            // PeerId is not known
            Vec::new()
        }
    }

    // ignore libp2p connections/streams
    fn inject_connected(&mut self, _: PeerId, _: ConnectedPoint) {}

    // ignore libp2p connections/streams
    fn inject_disconnected(&mut self, _: &PeerId, _: ConnectedPoint) {}

    // no libp2p discv5 events - event originate from the session_service.
    fn inject_node_event(
        &mut self,
        _: PeerId,
        _ev: <Self::ProtocolsHandler as ProtocolsHandler>::OutEvent,
    ) {
        void::unreachable(_ev)
    }

    fn poll(
        &mut self,
        params: &mut PollParameters<'_>,
    ) -> Async<
        NetworkBehaviourAction<
            <Self::ProtocolsHandler as ProtocolsHandler>::InEvent,
            Self::OutEvent,
        >,
    > {
        loop {
            // Drain queued events first.
            if !self.events.is_empty() {
                return Async::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)));
            }
            self.events.shrink_to_fit();

            // Process events from the session service
            loop {
                match self.service.poll() {
                    Async::Ready(event) => match event {
                        SessionEvent::Message(msg) => {
                            self.incomming_rpc(*msg);
                        }
                        SessionEvent::UpdatedEnr(enr) => {
                            debug!("ENR updated: {}", enr);
                            self.add_enr(*enr);
                        }
                        SessionEvent::WhoAreYouRequest {
                            src,
                            src_id,
                            auth_tag,
                        } => {
                            // check what our latest known ENR is for this node.
                            if let Some(known_enr) = self.find_enr(&src_id) {
                                self.service.send_whoareyou(
                                    src,
                                    &src_id,
                                    known_enr.seq,
                                    Some(known_enr),
                                    auth_tag,
                                );
                            } else {
                                // do not know of this peer
                                debug!("Peer Id unknown, requesting ENR. NodeId: {:?}", src_id);
                                self.service.send_whoareyou(src, &src_id, 0, None, auth_tag)
                            }
                        }
                        SessionEvent::RequestFailed(node_id, rpc_id) => {
                            self.rpc_failure(node_id, rpc_id);
                        }
                    },
                    Async::NotReady => break,
                }
            }

            // Drain applied pending entries from the routing table.
            if let Some(entry) = self.kbuckets.take_applied_pending() {
                let event = Discv5Event::NodeInserted {
                    node_id: entry.inserted.into_preimage(),
                    replaced: entry.evicted.map(|n| n.key.into_preimage()),
                };
                return Async::Ready(NetworkBehaviourAction::GenerateEvent(event));
            }

            // Handle active queries

            // If iterating finds a query that is finished, stores it here and stops looping.
            let mut finished_query = None;
            // If a query is waiting for an rpc to send, store it here and stop looping.
            let mut waiting_query = None;

            'queries_iter: for (&query_id, query) in self.active_queries.iter_mut() {
                let target = query.target().clone();
                loop {
                    match query.next() {
                        QueryState::Finished => {
                            finished_query = Some(query_id);
                            break 'queries_iter;
                        }
                        QueryState::Waiting(Some(return_peer)) => {
                            // break the loop to send the rpc request
                            waiting_query = Some((query_id, target, return_peer));
                            break 'queries_iter;
                        }
                        QueryState::Waiting(None) | QueryState::WaitingAtCapacity => break,
                    }
                }
            }

            if let Some((query_id, target, return_peer)) = waiting_query {
                self.send_rpc(&query_id, &target, &return_peer);
            } else if let Some(finished_query) = finished_query {
                let result = self
                    .active_queries
                    .remove(&finished_query)
                    .expect("finished_query was gathered when iterating active_queries; QED.")
                    .into_result();

                match result.target.query_type {
                    QueryType::FindNode(node_id) => {
                        let event = Discv5Event::FindNodeResult {
                            key: node_id,
                            closer_peers: result.closest_peers.collect(),
                        };
                        return Async::Ready(NetworkBehaviourAction::GenerateEvent(event));
                    }
                }
            } else {
                return Async::NotReady;
            }
        }
    }
}

// TODO: Potentially abstract ENR NodeId and use PeerId's for outer interface.
/// Event that can be produced by the `Discv5` behaviour.
#[derive(Debug)]
pub enum Discv5Event {
    /// Discovered nodes through discv5.
    Discovered {
        enr_id: NodeId,
        addresses: Vec<Multiaddr>,
        ty: ConnectionType,
    },
    EnrAdded {
        enr: Enr,
        replaced: Option<Enr>,
    },
    NodeInserted {
        node_id: NodeId,
        replaced: Option<NodeId>,
    },
    /// Result of a `FIND_NODE` iterative query.
    FindNodeResult {
        /// The key that we looked for in the query.
        key: NodeId,
        /// List of peers ordered from closest to furthest away.
        closer_peers: Vec<NodeId>,
    },
}

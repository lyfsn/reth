//! Wrapper around [`discv5::Discv5`].

use std::{fmt, net::IpAddr, sync::Arc, time::Duration};

use ::enr::Enr;
use alloy_rlp::Decodable;
use derive_more::{Constructor, Deref, DerefMut};
use enr::{uncompressed_to_compressed_id, EnrCombinedKeyWrapper};
use filter::{DefaultFilter, FilterDiscovered, FilterOutcome};
use futures::future::join_all;
use itertools::Itertools;
use reth_discv4::secp256k1::SecretKey;
use reth_net_common::discovery::{HandleDiscovery, NodeFromExternalSource};
use reth_primitives::{
    bytes::{Bytes, BytesMut},
    ForkId, NodeRecord, PeerId,
};
use tokio::{sync::mpsc, task};
use tracing::{debug, error, trace};

pub mod config;
pub mod downgrade_v4;
pub mod enr;
pub mod filter;

pub use discv5::{self, IpMode};

pub use config::{BootNode, DiscV5Config, DiscV5ConfigBuilder};
pub use downgrade_v4::{DiscV5WithV4Downgrade, MergedUpdateStream};
pub use enr::uncompressed_id_from_enr_pk;

/// Errors from using [`discv5::Discv5`] handle.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Failure adding node to [`discv5::Discv5`].
    #[error("failed adding node to discv5, {0}")]
    AddNodeToDiscv5Failed(&'static str),
    /// Missing key used to identify mempool network.
    #[error("fork id missing on enr, 'eth' key missing")]
    ForkIdMissing,
    /// Failed to decode [`ForkId`] rlp value.
    #[error("failed to decode fork id, 'eth': {0:?}")]
    ForkIdDecodeError(#[from] alloy_rlp::Error),
    /// Peer is unreachable over discovery.
    #[error("discovery socket missing, ENR: {0:?}")]
    UnreachableDiscovery(discv5::Enr),
    /// Peer is unreachable over mempool.
    #[error("mempool TCP socket missing, ENR: {0:?}")]
    UnreachableMempool(discv5::Enr),
    /// Peer is not using same IP version as local node in discovery.
    #[error("discovery socket is unsupported IP version, ENR: {0:?}, local ip mode: {1:?}")]
    IpVersionMismatchDiscovery(discv5::Enr, IpMode),
    /// Peer is not using same IP version as local node in mempool.
    #[error("mempool TCP socket is unsupported IP version, ENR: {0:?}, local ip mode: {1:?}")]
    IpVersionMismatchMempool(discv5::Enr, IpMode),
    /// Failed to initialize [`discv5::Discv5`].
    #[error("init failed, {0}")]
    InitFailure(&'static str),
    /// An error from underlying [`discv5::Discv5`] node.
    #[error("{0}")]
    Discv5Error(discv5::Error),
    /// An error from underlying [`discv5::Discv5`] node.
    #[error("{0}")]
    Discv5ErrorStr(&'static str),
}

/// Use API of [`discv5::Discv5`].
pub trait HandleDiscv5 {
    /// Filter type is constrained by [`DiscV5Config`].
    type Filter;

    /// Exposes API of [`discv5::Discv5`].
    fn with_discv5<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&DiscV5<Self::Filter>) -> R;

    /// Returns the [`IpMode`] of the local node.
    fn ip_mode(&self) -> IpMode;

    /// Returns the key to use to identify the [`ForkId`] kv-pair on the [`Enr`](discv5::Enr).
    fn fork_id_key(&self) -> &[u8];

    /// Returns the [`ForkId`] of the given [`Enr`](discv5::Enr), if field is set.
    fn get_fork_id<K: discv5::enr::EnrKey>(
        &self,
        enr: &discv5::enr::Enr<K>,
    ) -> Result<ForkId, Error> {
        let mut fork_id_bytes = enr.get(self.fork_id_key()).ok_or(Error::ForkIdMissing)?;

        Ok(ForkId::decode(&mut fork_id_bytes)?)
    }

    /// Tries to convert an [`Enr`](discv5::Enr) into the backwards compatible type [`NodeRecord`],
    /// w.r.t. local [`IpMode`].
    fn try_into_reachable(&self, enr: discv5::Enr) -> Result<NodeRecord, Error> {
        // todo: track unreachable with metrics
        if enr.udp4_socket().is_none() && enr.udp6_socket().is_none() {
            return Err(Error::UnreachableDiscovery(enr))
        }
        let Some(udp_socket) = self.ip_mode().get_contactable_addr(&enr) else {
            return Err(Error::IpVersionMismatchDiscovery(enr, self.ip_mode()))
        };
        // since we, on bootstrap, set tcp4 in local ENR for `IpMode::Dual`, we prefer tcp4 here
        // too
        let Some(tcp_port) = (match self.ip_mode() {
            IpMode::Ip4 | IpMode::DualStack => enr.tcp4(),
            IpMode::Ip6 => enr.tcp6(),
        }) else {
            return Err(Error::IpVersionMismatchMempool(enr, self.ip_mode()))
        };

        let id = uncompressed_id_from_enr_pk(&enr);

        Ok(NodeRecord { address: udp_socket.ip(), tcp_port, udp_port: udp_socket.port(), id })
    }

    /// Applies filtering rules on an ENR. Returns [`Ok`](FilterOutcome::Ok) if peer should be
    /// passed up to app, and [`Ignore`](FilterOutcome::Ignore) if peer should instead be dropped.
    fn filter_discovered_peer(&self, enr: &discv5::Enr) -> FilterOutcome;
}

/// Transparent wrapper around [`discv5::Discv5`].
#[derive(Deref, DerefMut, Clone, Constructor)]
pub struct DiscV5<T = DefaultFilter> {
    #[deref]
    #[deref_mut]
    discv5: Arc<discv5::Discv5>,
    ip_mode: IpMode,
    // Notify app of discovered nodes that don't have a TCP port set in their ENR. These nodes are
    // filtered out by default. allow_no_tcp_discovered_nodes: bool,
    fork_id_key: &'static [u8],
    /// Optionally filter discovered peers before passing up to app.
    filter_discovered_peer: T,
}

impl<T> DiscV5<T> {
    fn add_node(&self, node_record: NodeFromExternalSource) -> Result<(), Error> {
        let NodeFromExternalSource::Enr(enr) = node_record else {
            unreachable!("cannot convert `NodeRecord` type to `Enr` type")
        };
        let enr = enr.into();
        let EnrCombinedKeyWrapper(enr) = enr;
        self.add_enr(enr).map_err(Error::AddNodeToDiscv5Failed)
    }

    fn update_local_enr(&self, key: &[u8], rlp: &Bytes) {
        let Ok(key_str) = std::str::from_utf8(key) else {
            error!(target: "discv5",
                err="key not utf-8",
                "failed to update local enr"
            );
            return
        };
        if let Err(err) = self.enr_insert(key_str, rlp) {
            error!(target: "discv5",
                %err,
                "failed to update local enr"
            );
        }
    }

    /// Spawns [`discv5::Discv5`]. Returns [`discv5::Discv5`] handle in reth compatible wrapper type
    /// [`DiscV5`], a receiver of [`discv5::Event`]s from the underlying node, and the local
    /// [`Enr`](discv5::Enr) converted into the reth compatible [`NodeRecord`] type (used in
    /// [`Discv4`](reth_discv4::Discv4)).
    pub async fn start(
        sk: &SecretKey,
        discv5_config: DiscV5Config<T>,
    ) -> Result<(Self, mpsc::Receiver<discv5::Event>, NodeRecord), Error>
    where
        T: FilterDiscovered + Clone + Send + 'static,
    {
        //
        // 1. make local enr from listen config
        //
        let DiscV5Config {
            discv5_config,
            bootstrap_nodes,
            fork_id,
            tcp_port,
            other_enr_data,
            allow_no_tcp_discovered_nodes: _,
            self_lookup_interval,
            filter_discovered_peer,
        } = discv5_config;

        let (enr, bc_enr, ip_mode, chain) = {
            let mut builder = discv5::enr::Enr::builder();

            use discv5::ListenConfig::*;
            let ip_mode = match discv5_config.listen_config {
                Ipv4 { ip, port } => {
                    builder.ip4(ip);
                    builder.udp4(port);
                    builder.tcp4(tcp_port);

                    IpMode::Ip4
                }
                Ipv6 { ip, port } => {
                    builder.ip6(ip);
                    builder.udp6(port);
                    builder.tcp6(tcp_port);

                    IpMode::Ip6
                }
                DualStack { ipv4, ipv4_port, ipv6, ipv6_port } => {
                    builder.ip4(ipv4);
                    builder.udp4(ipv4_port);
                    builder.tcp4(tcp_port);

                    builder.ip6(ipv6);
                    builder.udp6(ipv6_port);

                    IpMode::DualStack
                }
            };

            // add fork id
            let (chain, fork) = fork_id;
            builder.add_value(chain, &alloy_rlp::encode(fork));

            // add other data
            for (key, value) in other_enr_data {
                builder.add_value(key, &alloy_rlp::encode(value));
            }

            // enr v4 not to get confused with discv4, independent versioning enr and
            // discovery
            let enr = builder.build(sk).expect("should build enr v4");
            let EnrCombinedKeyWrapper(enr) = enr.into();

            // backwards compatible enr
            let socket = ip_mode.get_contactable_addr(&enr).unwrap();
            let bc_enr = NodeRecord::from_secret_key(socket, sk);

            (enr, bc_enr, ip_mode, chain)
        };

        //
        // 3. start discv5
        //
        let sk = {
            let mut sk = *sk.as_ref();
            discv5::enr::CombinedKey::secp256k1_from_bytes(&mut sk).unwrap()
        };
        let mut discv5 = match discv5::Discv5::new(enr, sk, discv5_config) {
            Ok(discv5) => discv5,
            Err(err) => return Err(Error::InitFailure(err)),
        };
        discv5.start().await.map_err(Error::Discv5Error)?;

        // start discv5 updates stream
        let discv5_updates = discv5.event_stream().await.map_err(Error::Discv5Error)?;

        let discv5 = Arc::new(discv5);

        //
        // 4. add boot nodes
        //
        let mut enr_requests = vec![];
        for node in bootstrap_nodes {
            match node {
                BootNode::Enr(node) => {
                    if let Err(err) = discv5.add_enr(node) {
                        return Err(Error::Discv5ErrorStr(err))
                    }
                }
                BootNode::Enode(enode) => enr_requests.push(task::spawn({
                    let discv5 = discv5.clone();
                    async move {
                        if let Err(err) = discv5.request_enr(enode.clone()).await {
                            debug!(target: "net::discv5",
                                enode,
                                %err,
                                "failed adding boot node"
                            );
                        }
                    }
                })),
            }
        }
        _ = join_all(enr_requests);

        debug!(target: "net::discv5",
            connected_boot_nodes=format!("[{:#}]", discv5.with_kbuckets(|kbuckets| kbuckets
                .write()
                .iter()
                .filter(|entry| entry.status.is_connected())
                .map(|connected_peer| connected_peer.node.key.preimage()).copied().collect::<Vec<_>>()
            ).into_iter().format(", ")),
            "added boot nodes"
        );

        // initiate regular lookups to populate kbuckets
        task::spawn({
            let discv5 = discv5.clone();

            let local_node_id = discv5.local_enr().node_id();
            let self_lookup_interval = Duration::from_secs(self_lookup_interval);

            let filter = filter_discovered_peer.clone();
            let predicate = Box::new(move |enr: &discv5::Enr| -> bool {
                match filter.filter_discovered_peer(enr) {
                    FilterOutcome::Ok => true,
                    FilterOutcome::Ignore { reason } => {
                        trace!(target: "net::discv5",
                            ?enr,
                            reason,
                            "filtered out peer from lookup query"
                        );

                        false
                    }
                }
            });
            // todo: graceful shutdown

            async move {
                loop {
                    trace!(target: "net::discv5",
                        self_lookup_interval=format!("{:#?}", self_lookup_interval),
                        "starting periodic lookup query"
                    );
                    match discv5
                        .find_node_predicate(
                            local_node_id,
                            predicate.clone() as Box<dyn Fn(&discv5::Enr) -> bool + Send>,
                            discv5::kbucket::MAX_NODES_PER_BUCKET,
                        )
                        .await
                    {
                        Err(err) => trace!(target: "net::discv5",
                            self_lookup_interval=format!("{:#?}", self_lookup_interval),
                            %err,
                            "periodic lookup query failed"
                        ),
                        Ok(peers) => trace!(target: "net::discv5",
                            self_lookup_interval=format!("{:#?}", self_lookup_interval),
                            peers_count=peers.len(),
                            peers=format!("[{:#}]", peers.iter()
                                .map(|enr| enr.node_id()
                            ).format(", ")),
                            "peers returned by periodic lookup query"
                        ),
                    }

                    // `Discv5::connected_peers` can be subset of sessions, not all peers make it
                    // into kbuckets, e.g. incoming sessions from peers with
                    // unreachable enrs
                    debug!(target: "net::discv5",
                        connected_peers=discv5.connected_peers(),
                        "connected peers in routing table"
                    );
                    tokio::time::sleep(self_lookup_interval).await;
                }
            }
        });

        Ok((DiscV5::new(discv5, ip_mode, chain, filter_discovered_peer), discv5_updates, bc_enr))
    }
}

impl<T> fmt::Debug for DiscV5<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "{ .. }".fmt(f)
    }
}

impl<T> HandleDiscovery for DiscV5<T> {
    fn add_node_to_routing_table(
        &self,
        node_record: NodeFromExternalSource,
    ) -> Result<(), impl std::error::Error> {
        self.add_node(node_record)
    }

    fn set_eip868_in_local_enr(&self, key: Vec<u8>, rlp: Bytes) {
        self.update_local_enr(&key, &rlp)
    }

    fn encode_and_set_eip868_in_local_enr(&self, key: Vec<u8>, value: impl alloy_rlp::Encodable) {
        let mut buf = BytesMut::new();
        value.encode(&mut buf);
        self.set_eip868_in_local_enr(key, buf.freeze())
    }

    fn ban_peer_by_ip_and_node_id(&self, peer_id: PeerId, ip: IpAddr) {
        let node_id = uncompressed_to_compressed_id(peer_id);
        self.ban_node(&node_id, None);
        self.ban_peer_by_ip(ip);
    }

    fn ban_peer_by_ip(&self, ip: IpAddr) {
        self.ban_ip(ip, None);
    }

    fn node_record(&self) -> NodeRecord {
        let enr: Enr<_> = EnrCombinedKeyWrapper(self.local_enr()).into();
        enr.try_into().unwrap()
    }
}

impl<T> HandleDiscv5 for DiscV5<T>
where
    T: FilterDiscovered,
{
    type Filter = T;

    fn with_discv5<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Self) -> R,
    {
        f(self)
    }

    fn ip_mode(&self) -> IpMode {
        self.ip_mode
    }

    fn fork_id_key(&self) -> &[u8] {
        self.fork_id_key
    }

    fn filter_discovered_peer(&self, enr: &discv5::Enr) -> FilterOutcome {
        T::filter_discovered_peer(&self.filter_discovered_peer, enr)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use rand::thread_rng;
    use tracing::trace;

    use super::*;

    async fn start_discovery_node(
        udp_port_discv5: u16,
    ) -> (DiscV5, mpsc::Receiver<discv5::Event>, NodeRecord) {
        let secret_key = SecretKey::new(&mut thread_rng());

        let discv5_addr: SocketAddr = format!("127.0.0.1:{udp_port_discv5}").parse().unwrap();

        let discv5_listen_config = discv5::ListenConfig::from(discv5_addr);
        let discv5_config = DiscV5Config::builder()
            .discv5_config(discv5::ConfigBuilder::new(discv5_listen_config).build())
            .build();

        DiscV5::start(&secret_key, discv5_config).await.expect("should build discv5")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discv5() {
        reth_tracing::init_test_tracing();

        // rig test

        // rig node_1
        let (node_1, mut stream_1, _) = start_discovery_node(30344).await;
        let node_1_enr = node_1.with_discv5(|discv5| discv5.local_enr());

        // rig node_2
        let (node_2, mut stream_2, _) = start_discovery_node(30355).await;
        let node_2_enr = node_2.with_discv5(|discv5| discv5.local_enr());

        trace!(target: "net::discovery::tests",
            node_1_node_id=format!("{:#}", node_1_enr.node_id()),
            node_2_node_id=format!("{:#}", node_2_enr.node_id()),
            "started nodes"
        );

        // test

        // add node_2 to discovery handle of node_1 (should add node to discv5 kbuckets)
        let node_2_enr_reth_compatible_ty: Enr<SecretKey> =
            EnrCombinedKeyWrapper(node_2_enr.clone()).into();
        node_1
            .add_node_to_routing_table(NodeFromExternalSource::Enr(node_2_enr_reth_compatible_ty))
            .unwrap();

        // verify node_2 is in KBuckets of node_1:discv5
        assert!(
            node_1.with_discv5(|discv5| discv5.table_entries_id().contains(&node_2_enr.node_id()))
        );

        // manually trigger connection from node_1 to node_2
        node_1.with_discv5(|discv5| discv5.send_ping(node_2_enr.clone())).await.unwrap();

        // verify node_1:discv5 is connected to node_2:discv5 and vv
        let event_2_v5 = stream_2.recv().await.unwrap();
        let event_1_v5 = stream_1.recv().await.unwrap();
        matches!(
            event_1_v5,
            discv5::Event::SessionEstablished(node, socket) if node == node_2_enr && socket == node_2_enr.udp4_socket().unwrap().into()
        );
        matches!(
            event_2_v5,
            discv5::Event::SessionEstablished(node, socket) if node == node_1_enr && socket == node_1_enr.udp4_socket().unwrap().into()
        );

        // verify node_1 is in KBuckets of node_2:discv5
        let event_2_v5 = stream_2.recv().await.unwrap();
        matches!(
            event_2_v5,
            discv5::Event::NodeInserted { node_id, replaced } if node_id == node_1_enr.node_id() && replaced.is_none()
        );
    }
}

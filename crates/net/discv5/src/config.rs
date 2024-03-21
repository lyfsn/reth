//! Wrapper around [`discv5::Config`].

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
};

use discv5::ListenConfig;
use multiaddr::{Multiaddr, Protocol};
use reth_discv4::DEFAULT_DISCOVERY_PORT;
use reth_primitives::{AnyNode, Bytes, ForkId, NodeRecord};

use crate::enr::uncompressed_to_multiaddr_id;

/// Default interval in seconds at which to run a self-lookup up query.
///
/// Default is 60 seconds.
const DEFAULT_SECONDS_SELF_LOOKUP_INTERVAL: u64 = 60;

/// Builds a [`DiscV5Config`].
#[derive(Debug, Default)]
pub struct DiscV5ConfigBuilder {
    /// Config used by [`discv5::Discv5`]. Contains the discovery listen socket.
    discv5_config: Option<discv5::Config>,
    /// Nodes to boot from.
    bootstrap_nodes: HashSet<BootNode>,
    /// [`ForkId`] to set in local node record.
    fork_id: Option<ForkId>,
    /// Mempool TCP port to advertise.
    tcp_port: Option<u16>,
    /// Additional kv-pairs that should be advertised to peers by including in local node record.
    other_enr_data: Vec<(&'static str, Bytes)>,
    /// Allow no TCP port advertised by discovered nodes. Disallowed by default.
    allow_no_tcp_discovered_nodes: bool,
    /// Interval in seconds at which to run a lookup up query with local node ID as target, to
    /// populate kbuckets.
    self_lookup_interval: Option<u64>,
}

impl DiscV5ConfigBuilder {
    /// Returns a new builder, with all fields set like given instance.
    pub fn new_from(discv5_config: DiscV5Config) -> Self {
        let DiscV5Config {
            discv5_config,
            bootstrap_nodes,
            fork_id,
            tcp_port,
            other_enr_data,
            allow_no_tcp_discovered_nodes,
            self_lookup_interval: lookup_interval,
        } = discv5_config;

        Self {
            discv5_config: Some(discv5_config),
            bootstrap_nodes,
            fork_id,
            tcp_port: Some(tcp_port),
            other_enr_data,
            allow_no_tcp_discovered_nodes,
            self_lookup_interval: Some(lookup_interval),
        }
    }

    /// Set [`discv5::Config`], which contains the [`discv5::Discv5`] listen socket.
    pub fn discv5_config(mut self, discv5_config: discv5::Config) -> Self {
        self.discv5_config = Some(discv5_config);
        self
    }

    /// Adds a boot node. Returns `true` if set didn't already contain the node.
    pub fn add_boot_node(mut self, node: discv5::Enr) -> Self {
        self.bootstrap_nodes.insert(BootNode::Enr(node));
        self
    }

    /// Adds multiple boot nodes.
    pub fn add_boot_nodes(mut self, nodes: impl IntoIterator<Item = discv5::Enr>) -> Self {
        self.bootstrap_nodes.extend(nodes.into_iter().map(|node| BootNode::Enr(node)));
        self
    }

    /// Parses a comma-separated list of serialized [`Enr`](discv5::Enr)s, signed node records, and
    /// adds any successfully deserialized records to boot nodes.
    pub fn add_serialized_boot_nodes(mut self, enrs: &str) -> Self {
        let bootstrap_nodes = &mut self.bootstrap_nodes;
        for res in enrs.split(&[',']).map(|record| record.trim().parse::<discv5::Enr>()) {
            if let Ok(node) = res {
                bootstrap_nodes.insert(BootNode::Enr(node));
            }
        }
        self
    }

    /// Adds a comma-separated list of enodes, unsigned node records, to boot nodes.
    pub fn add_enode_boot_nodes(mut self, enodes: &'static str) -> Self {
        let bootstrap_nodes = &mut self.bootstrap_nodes;

        for node in enodes.split(&[',']).into_iter() {
            if let Ok(AnyNode::NodeRecord(NodeRecord { address, udp_port, id, .. })) = node.parse()
            {
                let mut multi_address = Multiaddr::empty();
                match address {
                    IpAddr::V4(ip) => multi_address.push(Protocol::Ip4(ip)),
                    IpAddr::V6(ip) => multi_address.push(Protocol::Ip6(ip)),
                }

                multi_address.push(Protocol::Udp(udp_port));
                let id = uncompressed_to_multiaddr_id(id);
                multi_address.push(Protocol::P2p(id));

                bootstrap_nodes.insert(BootNode::Enode(multi_address.to_string()));
            }
        }
        self
    }

    /// Set [`ForkId`] to include in the local [`Enr`](discv5::enr::Enr).
    pub fn fork_id(mut self, fork_id: ForkId) -> Self {
        self.fork_id = Some(fork_id);
        self
    }

    /// Sets the tcp port to advertise in the local [`Enr`](discv5::enr::Enr).
    pub fn tcp_port(mut self, port: u16) -> Self {
        self.tcp_port = Some(port);
        self
    }

    /// Adds an additional kv-pair to include in the local [`Enr`](discv5::enr::Enr).
    pub fn add_enr_kv_pair(mut self, kv_pair: (&'static str, Bytes)) -> Self {
        self.other_enr_data.push(kv_pair);
        self
    }

    /// Allows discovered nodes without tcp port set in their ENR to be passed from
    /// [`discv5::Discv5`] up to the app.
    pub fn allow_no_tcp_discovered_nodes(mut self) -> Self {
        self.allow_no_tcp_discovered_nodes = true;
        self
    }

    /// Returns a new [`DiscV5Config`].
    pub fn build(self) -> DiscV5Config {
        let Self {
            discv5_config,
            bootstrap_nodes,
            fork_id,
            tcp_port,
            other_enr_data,
            allow_no_tcp_discovered_nodes,
            self_lookup_interval: lookup_interval,
        } = self;

        let discv5_config = discv5_config
            .unwrap_or_else(|| discv5::ConfigBuilder::new(ListenConfig::default()).build());

        let tcp_port = tcp_port.unwrap_or(DEFAULT_DISCOVERY_PORT);

        let lookup_interval = lookup_interval.unwrap_or(DEFAULT_SECONDS_SELF_LOOKUP_INTERVAL);

        DiscV5Config {
            discv5_config,
            bootstrap_nodes,
            fork_id,
            tcp_port,
            other_enr_data,
            allow_no_tcp_discovered_nodes,
            self_lookup_interval: lookup_interval,
        }
    }
}

/// Config used to bootstrap [`discv5::Discv5`].
#[derive(Debug)]
pub struct DiscV5Config {
    /// Config used by [`discv5::Discv5`]. Contains the [`ListenConfig`], with the discovery listen
    /// socket.
    pub(super) discv5_config: discv5::Config,
    /// Nodes to boot from.
    pub(super) bootstrap_nodes: HashSet<BootNode>,
    /// [`ForkId`] to set in local node record.
    pub(super) fork_id: Option<ForkId>,
    /// Mempool TCP port to advertise.
    pub(super) tcp_port: u16,
    /// Additional kv-pairs to include in local node record.
    pub(super) other_enr_data: Vec<(&'static str, Bytes)>,
    /// Allow no TCP port advertised by discovered nodes. Disallowed by default.
    pub(super) allow_no_tcp_discovered_nodes: bool,
    /// Interval in seconds at which to run a lookup up query with local node ID as target, to
    /// populate kbuckets.
    pub(super) self_lookup_interval: u64,
}

impl DiscV5Config {
    /// Returns a new [`DiscV5ConfigBuilder`].
    pub fn builder() -> DiscV5ConfigBuilder {
        DiscV5ConfigBuilder::default()
    }

    /// Returns the socket contained in the [`discv5::Config`]. Returns the IPv6 socket, if both
    /// IPv4 and v6 are configured.
    pub fn socket(&self) -> SocketAddr {
        match self.discv5_config.listen_config {
            ListenConfig::Ipv4 { ip, port } => (ip, port).into(),
            ListenConfig::Ipv6 { ip, port } => (ip, port).into(),
            ListenConfig::DualStack { ipv6, ipv6_port, .. } => (ipv6, ipv6_port).into(),
        }
    }
}

/// A boot node can be added either as a string in either 'enode' URL scheme or serialized from
/// [`Enr`](discv5::Enr) type.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum BootNode {
    /// An unsigned node record.
    Enode(String),
    /// A signed node record.
    Enr(discv5::Enr),
}

#[cfg(test)]
mod test {
    use std::net::SocketAddrV4;

    use reth_primitives::hex;

    use super::*;

    #[test]
    fn parse_boot_nodes() {
        const OP_SEPOLIA_NODE_P2P_BOOTNODES: &'static str ="enr:-J64QBwRIWAco7lv6jImSOjPU_W266lHXzpAS5YOh7WmgTyBZkgLgOwo_mxKJq3wz2XRbsoBItbv1dCyjIoNq67mFguGAYrTxM42gmlkgnY0gmlwhBLSsHKHb3BzdGFja4S0lAUAiXNlY3AyNTZrMaEDmoWSi8hcsRpQf2eJsNUx-sqv6fH4btmo2HsAzZFAKnKDdGNwgiQGg3VkcIIkBg,enr:-J64QFa3qMsONLGphfjEkeYyF6Jkil_jCuJmm7_a42ckZeUQGLVzrzstZNb1dgBp1GGx9bzImq5VxJLP-BaptZThGiWGAYrTytOvgmlkgnY0gmlwhGsV-zeHb3BzdGFja4S0lAUAiXNlY3AyNTZrMaEDahfSECTIS_cXyZ8IyNf4leANlZnrsMEWTkEYxf4GMCmDdGNwgiQGg3VkcIIkBg";

        let config = DiscV5Config::builder()
            .add_serialized_boot_nodes(OP_SEPOLIA_NODE_P2P_BOOTNODES)
            .build();

        let socket_1 = "18.210.176.114:9222".parse::<SocketAddrV4>().unwrap();
        let socket_2 = "107.21.251.55:9222".parse::<SocketAddrV4>().unwrap();

        for node in config.bootstrap_nodes {
            let BootNode::Enr(node) = node else { panic!() };
            assert!(
                socket_1 == node.udp4_socket().unwrap() && socket_1 == node.tcp4_socket().unwrap() ||
                    socket_2 == node.udp4_socket().unwrap() &&
                        socket_2 == node.tcp4_socket().unwrap()
            );
            assert_eq!("84b4940500", hex::encode(node.get_raw_rlp("opstack").unwrap()));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parse_enodes() {
        const OP_GETH_BOOTNODES: &str ="enode://87a32fd13bd596b2ffca97020e31aef4ddcc1bbd4b95bb633d16c1329f654f34049ed240a36b449fda5e5225d70fe40bc667f53c304b71f8e68fc9d448690b51@3.231.138.188:30301,enode://ca21ea8f176adb2e229ce2d700830c844af0ea941a1d8152a9513b966fe525e809c3a6c73a2c18a12b74ed6ec4380edf91662778fe0b79f6a591236e49e176f9@184.72.129.189:30301,enode://acf4507a211ba7c1e52cdf4eef62cdc3c32e7c9c47998954f7ba024026f9a6b2150cd3f0b734d9c78e507ab70d59ba61dfe5c45e1078c7ad0775fb251d7735a2@3.220.145.177:30301,enode://8a5a5006159bf079d06a04e5eceab2a1ce6e0f721875b2a9c96905336219dbe14203d38f70f3754686a6324f786c2f9852d8c0dd3adac2d080f4db35efc678c5@3.231.11.52:30301,enode://cdadbe835308ad3557f9a1de8db411da1a260a98f8421d62da90e71da66e55e98aaa8e90aa7ce01b408a54e4bd2253d701218081ded3dbe5efbbc7b41d7cef79@54.198.153.150:30301";

        let config = DiscV5Config::builder().add_enode_boot_nodes(OP_GETH_BOOTNODES).build();

        const MUTLI_ADDRESSES: &str = "/ip4/184.72.129.189/udp/30301/p2p/16Uiu2HAmSG2hdLwyQHQmG4bcJBgD64xnW63WMTLcrNq6KoZREfGb,/ip4/3.231.11.52/udp/30301/p2p/16Uiu2HAmMy4V8bi3XP7KDfSLQcLACSvTLroRRwEsTyFUKo8NCkkp,/ip4/54.198.153.150/udp/30301/p2p/16Uiu2HAmSVsb7MbRf1jg3Dvd6a3n5YNqKQwn1fqHCFgnbqCsFZKe,/ip4/3.220.145.177/udp/30301/p2p/16Uiu2HAm74pBDGdQ84XCZK27GRQbGFFwQ7RsSqsPwcGmCR3Cwn3B,/ip4/3.231.138.188/udp/30301/p2p/16Uiu2HAmMnTiJwgFtSVGV14ZNpwAvS1LUoF4pWWeNtURuV6C3zYB";

        for node in MUTLI_ADDRESSES.split(&[',']) {
            assert!(config.bootstrap_nodes.contains(&BootNode::Enode(node.to_string())));
        }
    }
}

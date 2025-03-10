// Copyright 2024 The Tari Project
// SPDX-License-Identifier: BSD-3-Clause

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use libp2p::{
    futures::StreamExt,
    gossipsub,
    gossipsub::{IdentTopic, Message, PublishError},
    identity::Keypair,
    kad,
    kad::{store::MemoryStore, Event, Mode},
    mdns,
    mdns::tokio::Tokio,
    multiaddr::Protocol,
    noise, request_response,
    request_response::{cbor, ResponseChannel},
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, StreamProtocol, Swarm,
};
use log::{debug, error, info, warn};
use tari_common::configuration::Network;
use tari_utilities::hex::Hex;
use tokio::{
    fs::File,
    io,
    io::{AsyncReadExt, AsyncWriteExt},
    select,
    sync::{broadcast, broadcast::error::RecvError, mpsc},
};

use crate::{
    server::{
        config,
        p2p::{
            client, messages,
            messages::{
                PeerInfo, ShareChainSyncRequest, ShareChainSyncResponse, ValidateBlockRequest, ValidateBlockResult,
            },
            peer_store::PeerStore,
            Error, LibP2PError, ServiceClient, ServiceClientChannels,
        },
    },
    sharechain::{block::Block, ShareChain},
};

const PEER_INFO_TOPIC: &str = "peer_info";
const BLOCK_VALIDATION_REQUESTS_TOPIC: &str = "block_validation_requests";
const BLOCK_VALIDATION_RESULTS_TOPIC: &str = "block_validation_results";
const NEW_BLOCK_TOPIC: &str = "new_block";
const SHARE_CHAIN_SYNC_REQ_RESP_PROTOCOL: &str = "/share_chain_sync/1";
const LOG_TARGET: &str = "p2p_service";
const STABLE_PRIVATE_KEY_FILE: &str = "p2pool_private.key";

#[derive(Clone, Debug)]
pub struct Config {
    pub seed_peers: Vec<String>,
    pub client: client::ClientConfig,
    pub peer_info_publish_interval: Duration,
    pub stable_peer: bool,
    pub private_key_folder: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seed_peers: vec![],
            client: client::ClientConfig::default(),
            peer_info_publish_interval: Duration::from_secs(5),
            stable_peer: false,
            private_key_folder: PathBuf::from("."),
        }
    }
}

#[derive(NetworkBehaviour)]
pub struct ServerNetworkBehaviour {
    pub mdns: mdns::Behaviour<Tokio>,
    pub gossipsub: gossipsub::Behaviour,
    pub share_chain_sync: cbor::Behaviour<ShareChainSyncRequest, ShareChainSyncResponse>,
    pub kademlia: kad::Behaviour<MemoryStore>,
}

/// Service is the implementation that holds every peer-to-peer related logic
/// that makes sure that all the communications, syncing, broadcasting etc... are done.
pub struct Service<S>
where
    S: ShareChain + Send + Sync + 'static,
{
    swarm: Swarm<ServerNetworkBehaviour>,
    port: u16,
    share_chain: Arc<S>,
    peer_store: Arc<PeerStore>,
    config: Config,

    // service client related channels
    // TODO: consider mpsc channels instead of broadcast to not miss any message (might drop)
    client_validate_block_req_tx: broadcast::Sender<ValidateBlockRequest>,
    client_validate_block_req_rx: broadcast::Receiver<ValidateBlockRequest>,
    client_validate_block_res_txs: Vec<mpsc::UnboundedSender<ValidateBlockResult>>,
    client_broadcast_block_tx: broadcast::Sender<Block>,
    client_broadcast_block_rx: broadcast::Receiver<Block>,
    client_peer_changes_tx: broadcast::Sender<()>,
    client_peer_changes_rx: broadcast::Receiver<()>,
}

impl<S> Service<S>
where
    S: ShareChain + Send + Sync + 'static,
{
    /// Constructs a new Service from the provided config.
    /// It also instantiates libp2p swarm inside.
    pub async fn new(config: &config::Config, share_chain: Arc<S>) -> Result<Self, Error> {
        let swarm = Self::new_swarm(config).await?;
        let peer_store = Arc::new(PeerStore::new(&config.peer_store));

        // client related channels
        let (validate_req_tx, validate_req_rx) = broadcast::channel::<ValidateBlockRequest>(1000);
        let (broadcast_block_tx, broadcast_block_rx) = broadcast::channel::<Block>(1000);
        let (peer_changes_tx, peer_changes_rx) = broadcast::channel::<()>(1000);

        Ok(Self {
            swarm,
            port: config.p2p_port,
            share_chain,
            peer_store,
            config: config.p2p_service.clone(),
            client_validate_block_req_tx: validate_req_tx,
            client_validate_block_req_rx: validate_req_rx,
            client_validate_block_res_txs: vec![],
            client_broadcast_block_tx: broadcast_block_tx,
            client_broadcast_block_rx: broadcast_block_rx,
            client_peer_changes_tx: peer_changes_tx,
            client_peer_changes_rx: peer_changes_rx,
        })
    }

    /// Generates or reads libp2p private key if stable_peer is set to true otherwise returns a random key.
    /// Using this method we can be sure that our Peer ID remains the same across restarts in case of
    /// stable_peer is set to true.
    async fn keypair(config: &Config) -> Result<Keypair, Error> {
        if !config.stable_peer {
            return Ok(Keypair::generate_ed25519());
        }

        // if we have a saved private key, just use it
        let mut content = vec![];
        let mut key_path = config.private_key_folder.clone();
        key_path.push(STABLE_PRIVATE_KEY_FILE);

        if let Ok(mut file) = File::open(key_path.clone()).await {
            if file.read_to_end(&mut content).await.is_ok() {
                return Keypair::from_protobuf_encoding(content.as_slice())
                    .map_err(|error| Error::LibP2P(LibP2PError::KeyDecoding(error)));
            }
        }

        // otherwise create a new one
        let key_pair = Keypair::generate_ed25519();
        let mut new_private_key_file = File::create_new(key_path)
            .await
            .map_err(|error| Error::LibP2P(LibP2PError::IO(error)))?;
        new_private_key_file
            .write_all(
                key_pair
                    .to_protobuf_encoding()
                    .map_err(|error| Error::LibP2P(LibP2PError::KeyDecoding(error)))?
                    .as_slice(),
            )
            .await
            .map_err(|error| Error::LibP2P(LibP2PError::IO(error)))?;

        Ok(key_pair)
    }

    /// Creates a new swarm from the provided config
    async fn new_swarm(config: &config::Config) -> Result<Swarm<ServerNetworkBehaviour>, Error> {
        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(Self::keypair(&config.p2p_service).await?)
            .with_tokio()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)
            .map_err(|e| Error::LibP2P(LibP2PError::Noise(e)))?
            .with_behaviour(move |key_pair| {
                // gossipsub
                let message_id_fn = |message: &gossipsub::Message| {
                    let mut s = DefaultHasher::new();
                    if let Some(soure_peer) = message.source {
                        soure_peer.to_bytes().hash(&mut s);
                    }
                    message.data.hash(&mut s);
                    gossipsub::MessageId::from(s.finish().to_string())
                };
                let gossipsub_config = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(10))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .message_id_fn(message_id_fn)
                    .build()
                    .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?;
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key_pair.clone()),
                    gossipsub_config,
                )?;

                Ok(ServerNetworkBehaviour {
                    gossipsub,
                    mdns: mdns::Behaviour::new(mdns::Config::default(), key_pair.public().to_peer_id())
                        .map_err(|e| Error::LibP2P(LibP2PError::IO(e)))?,
                    share_chain_sync: cbor::Behaviour::<ShareChainSyncRequest, ShareChainSyncResponse>::new(
                        [(
                            StreamProtocol::new(SHARE_CHAIN_SYNC_REQ_RESP_PROTOCOL),
                            request_response::ProtocolSupport::Full,
                        )],
                        request_response::Config::default(),
                    ),
                    kademlia: kad::Behaviour::new(
                        key_pair.public().to_peer_id(),
                        MemoryStore::new(key_pair.public().to_peer_id()),
                    ),
                })
            })
            .map_err(|e| Error::LibP2P(LibP2PError::Behaviour(e.to_string())))?
            .with_swarm_config(|c| c.with_idle_connection_timeout(config.idle_connection_timeout))
            .build();

        swarm.behaviour_mut().kademlia.set_mode(Some(Mode::Server));

        Ok(swarm)
    }

    /// Creates a new client for this service, it is thread safe (Send + Sync).
    /// Any amount of clients can be created, no need to share the same one across many components.
    pub fn client(&mut self) -> ServiceClient {
        let (validate_res_tx, validate_res_rx) = mpsc::unbounded_channel::<ValidateBlockResult>();
        self.client_validate_block_res_txs.push(validate_res_tx);

        ServiceClient::new(
            ServiceClientChannels::new(
                self.client_validate_block_req_tx.clone(),
                validate_res_rx,
                self.client_broadcast_block_tx.clone(),
                self.client_peer_changes_rx.resubscribe(),
            ),
            self.peer_store.clone(),
            self.config.client.clone(),
        )
    }

    /// Handles block validation requests coming from Service clients.
    /// All the requests from clients are sent to [`BLOCK_VALIDATION_REQUESTS_TOPIC`].
    async fn handle_client_validate_block_request(&mut self, result: Result<ValidateBlockRequest, RecvError>) {
        match result {
            Ok(request) => {
                let request_raw_result: Result<Vec<u8>, Error> = request.try_into();
                match request_raw_result {
                    Ok(request_raw) => {
                        if let Err(error) = self.swarm.behaviour_mut().gossipsub.publish(
                            IdentTopic::new(Self::topic_name(BLOCK_VALIDATION_REQUESTS_TOPIC)),
                            request_raw,
                        ) {
                            error!(target: LOG_TARGET, "Failed to send block validation request: {error:?}");
                        }
                    },
                    Err(error) => {
                        error!(target: LOG_TARGET, "Failed to convert block validation request to bytes: {error:?}");
                    },
                }
            },
            Err(error) => {
                error!(target: LOG_TARGET, "Block validation request receive error: {error:?}");
            },
        }
    }

    /// Sending validation result for a block to [`BLOCK_VALIDATION_RESULTS_TOPIC`] gossipsub topic.
    async fn send_block_validation_result(&mut self, result: ValidateBlockResult) {
        let result_raw_result: Result<Vec<u8>, Error> = result.try_into();
        match result_raw_result {
            Ok(result_raw) => {
                if let Err(error) = self.swarm.behaviour_mut().gossipsub.publish(
                    IdentTopic::new(Self::topic_name(BLOCK_VALIDATION_RESULTS_TOPIC)),
                    result_raw,
                ) {
                    error!(target: LOG_TARGET, "Failed to publish block validation result: {error:?}");
                }
            },
            Err(error) => {
                error!(target: LOG_TARGET, "Failed to convert block validation result to bytes: {error:?}");
            },
        }
    }

    /// Broadcasting current peer's information ([`PeerInfo`]) to other peers in the network
    /// by sending this data to [`PEER_INFO_TOPIC`] gossipsub topic.
    async fn broadcast_peer_info(&mut self) -> Result<(), Error> {
        // get peer info
        let share_chain = self.share_chain.clone();
        let current_height = share_chain.tip_height().await.map_err(Error::ShareChain)?;
        let peer_info_raw: Vec<u8> = PeerInfo::new(current_height).try_into()?;

        // broadcast peer info
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(IdentTopic::new(Self::topic_name(PEER_INFO_TOPIC)), peer_info_raw)
            .map_err(|error| Error::LibP2P(LibP2PError::Publish(error)))?;

        Ok(())
    }

    /// Broadcasting a new mined [`Block`] to the network (assume it is already validated with the network).
    async fn broadcast_block(&mut self, result: Result<Block, RecvError>) {
        match result {
            Ok(block) => {
                let block_raw_result: Result<Vec<u8>, Error> = block.try_into();
                match block_raw_result {
                    Ok(block_raw) => {
                        match self
                            .swarm
                            .behaviour_mut()
                            .gossipsub
                            .publish(IdentTopic::new(Self::topic_name(NEW_BLOCK_TOPIC)), block_raw)
                            .map_err(|error| Error::LibP2P(LibP2PError::Publish(error)))
                        {
                            Ok(_) => {},
                            Err(error) => {
                                error!(target: LOG_TARGET, "Failed to broadcast new block: {error:?}")
                            },
                        }
                    },
                    Err(error) => {
                        error!(target: LOG_TARGET, "Failed to convert block to bytes: {error:?}")
                    },
                }
            },
            Err(error) => error!(target: LOG_TARGET, "Failed to receive new block: {error:?}"),
        }
    }

    /// Generates the gossip sub topic names based on the current Tari network to avoid mixing up
    /// blocks and peers with different Tari networks.
    fn topic_name(topic: &str) -> String {
        let network = Network::get_current_or_user_setting_or_default().to_string();
        format!("{network}_{topic}")
    }

    /// Subscribing to a gossipsub topic.
    fn subscribe(&mut self, topic: &str) {
        self.swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&IdentTopic::new(Self::topic_name(topic)))
            .expect("must be subscribed to topic");
    }

    /// Subscribes to all topics we need.
    fn subscribe_to_topics(&mut self) {
        self.subscribe(PEER_INFO_TOPIC);
        self.subscribe(BLOCK_VALIDATION_REQUESTS_TOPIC);
        self.subscribe(BLOCK_VALIDATION_RESULTS_TOPIC);
        self.subscribe(NEW_BLOCK_TOPIC);
    }

    /// Main method to handle any message comes from gossipsub.
    async fn handle_new_gossipsub_message(&mut self, message: Message) {
        let peer = message.source;
        if peer.is_none() {
            warn!("Message source is not set! {:?}", message);
            return;
        }
        let peer = peer.unwrap();

        let topic = message.topic.to_string();

        match topic {
            topic if topic == Self::topic_name(PEER_INFO_TOPIC) => match messages::PeerInfo::try_from(message) {
                Ok(payload) => {
                    debug!(target: LOG_TARGET, "New peer info: {peer:?} -> {payload:?}");
                    self.peer_store.add(peer, payload).await;
                    if let Some(tip) = self.peer_store.tip_of_block_height().await {
                        if let Ok(curr_height) = self.share_chain.tip_height().await {
                            if curr_height < tip.height {
                                self.sync_share_chain().await;
                            }
                        }
                    }
                },
                Err(error) => {
                    error!(target: LOG_TARGET, "Can't deserialize peer info payload: {:?}", error);
                },
            },
            topic if topic == Self::topic_name(BLOCK_VALIDATION_REQUESTS_TOPIC) => {
                match messages::ValidateBlockRequest::try_from(message) {
                    Ok(payload) => {
                        debug!(target: LOG_TARGET, "Block validation request: {payload:?}");

                        let validate_result = self.share_chain.validate_block(&payload.block()).await;
                        let mut valid = false;
                        if let Ok(is_valid) = validate_result {
                            valid = is_valid;
                        }

                        // TODO: Generate partial schnorr signature to prove that current peer validated the block
                        // (using peer's private key and broadcast public key vie PeerInfo)
                        // TODO: to be able to verify at other peers.
                        // TODO: Validate whether new block includes all the shares (generate shares until height of
                        // new_block.height - 1) TODO: by generating a new block and check
                        // kernels/outputs whether they are the same or not. TODO: Validating
                        // new blocks version 2 would be to send a proof that was generated from the shares.

                        let validate_result =
                            ValidateBlockResult::new(*self.swarm.local_peer_id(), payload.block(), valid);
                        self.send_block_validation_result(validate_result).await;
                    },
                    Err(error) => {
                        error!(target: LOG_TARGET, "Can't deserialize block validation request payload: {:?}", error);
                    },
                }
            },
            topic if topic == Self::topic_name(BLOCK_VALIDATION_RESULTS_TOPIC) => {
                match messages::ValidateBlockResult::try_from(message) {
                    Ok(payload) => {
                        let mut senders_to_delete = vec![];
                        for (i, sender) in self.client_validate_block_res_txs.iter().enumerate() {
                            if let Err(error) = sender.send(payload.clone()) {
                                error!(target: LOG_TARGET, "Failed to send block validation result to client: {error:?}");
                                senders_to_delete.push(i);
                            }
                        }
                        senders_to_delete.iter().for_each(|i| {
                            self.client_validate_block_res_txs.remove(*i);
                        });
                    },
                    Err(error) => {
                        error!(target: LOG_TARGET, "Can't deserialize block validation request payload: {:?}", error);
                    },
                }
            },
            // TODO: send a signature that proves that the actual block was coming from this peer
            topic if topic == Self::topic_name(NEW_BLOCK_TOPIC) => match Block::try_from(message) {
                Ok(payload) => {
                    info!(target: LOG_TARGET,"🆕 New block from broadcast: {:?}", &payload.hash().to_hex());
                    if let Err(error) = self.share_chain.submit_block(&payload).await {
                        error!(target: LOG_TARGET, "Could not add new block to local share chain: {error:?}");
                    }
                },
                Err(error) => {
                    error!(target: LOG_TARGET, "Can't deserialize broadcast block payload: {:?}", error);
                },
            },
            _ => {
                warn!(target: LOG_TARGET, "Unknown topic {topic:?}!");
            },
        }
    }

    /// Handles share chain sync request (coming from other peer).
    async fn handle_share_chain_sync_request(
        &mut self,
        channel: ResponseChannel<ShareChainSyncResponse>,
        request: ShareChainSyncRequest,
    ) {
        debug!(target: LOG_TARGET, "Incoming Share chain sync request: {request:?}");
        match self.share_chain.blocks(request.from_height).await {
            Ok(blocks) => {
                if self
                    .swarm
                    .behaviour_mut()
                    .share_chain_sync
                    .send_response(channel, ShareChainSyncResponse::new(blocks.clone()))
                    .is_err()
                {
                    error!(target: LOG_TARGET, "Failed to send block sync response");
                }
            },
            Err(error) => error!(target: LOG_TARGET, "Failed to get blocks from height: {error:?}"),
        }
    }

    /// Handle share chain sync response.
    /// All the responding blocks will be tried to put into local share chain.
    async fn handle_share_chain_sync_response(&mut self, response: ShareChainSyncResponse) {
        debug!(target: LOG_TARGET, "Share chain sync response: {response:?}");
        if let Err(error) = self.share_chain.submit_blocks(response.blocks, true).await {
            error!(target: LOG_TARGET, "Failed to add synced blocks to share chain: {error:?}");
            self.sync_share_chain().await;
        }
    }

    /// Trigger share chai sync with another peer with the highest known block height.
    async fn sync_share_chain(&mut self) {
        debug!(target: LOG_TARGET, "Syncing share chain...");
        while self.peer_store.tip_of_block_height().await.is_none() {} // waiting for the highest blockchain
        match self.peer_store.tip_of_block_height().await {
            Some(result) => {
                debug!(target: LOG_TARGET, "Found highest block height: {result:?}");
                match self.share_chain.tip_height().await {
                    Ok(tip) => {
                        debug!(target: LOG_TARGET, "Send share chain sync request: {result:?}");
                        self.swarm
                            .behaviour_mut()
                            .share_chain_sync
                            .send_request(&result.peer_id, ShareChainSyncRequest::new(tip));
                    },
                    Err(error) => {
                        error!(target: LOG_TARGET, "Failed to get latest height of share chain: {error:?}")
                    },
                }
            },
            None => {
                error!(target: LOG_TARGET, "Failed to get peer with highest share chain height!")
            },
        }
    }

    /// Main method to handle libp2p events.
    async fn handle_event(&mut self, event: SwarmEvent<ServerNetworkBehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                info!(target: LOG_TARGET, "Listening on {address:?}");
            },
            SwarmEvent::Behaviour(event) => match event {
                ServerNetworkBehaviourEvent::Mdns(mdns_event) => match mdns_event {
                    mdns::Event::Discovered(peers) => {
                        for (peer, addr) in peers {
                            self.swarm.add_peer_address(peer, addr);
                            self.swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                        }
                    },
                    mdns::Event::Expired(peers) => {
                        for (peer, _addr) in peers {
                            self.swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer);
                        }
                    },
                },
                ServerNetworkBehaviourEvent::Gossipsub(event) => match event {
                    gossipsub::Event::Message {
                        message,
                        message_id: _message_id,
                        propagation_source: _propagation_source,
                    } => {
                        self.handle_new_gossipsub_message(message).await;
                    },
                    gossipsub::Event::Subscribed { .. } => {},
                    gossipsub::Event::Unsubscribed { .. } => {},
                    gossipsub::Event::GossipsubNotSupported { .. } => {},
                },
                ServerNetworkBehaviourEvent::ShareChainSync(event) => match event {
                    request_response::Event::Message { peer: _peer, message } => match message {
                        request_response::Message::Request {
                            request_id: _request_id,
                            request,
                            channel,
                        } => {
                            self.handle_share_chain_sync_request(channel, request).await;
                        },
                        request_response::Message::Response {
                            request_id: _request_id,
                            response,
                        } => {
                            self.handle_share_chain_sync_response(response).await;
                        },
                    },
                    request_response::Event::OutboundFailure { peer, error, .. } => {
                        error!(target: LOG_TARGET, "REQ-RES outbound failure: {peer:?} -> {error:?}");
                    },
                    request_response::Event::InboundFailure { peer, error, .. } => {
                        error!(target: LOG_TARGET, "REQ-RES inbound failure: {peer:?} -> {error:?}");
                    },
                    request_response::Event::ResponseSent { .. } => {},
                },
                ServerNetworkBehaviourEvent::Kademlia(event) => match event {
                    Event::RoutingUpdated {
                        peer,
                        old_peer,
                        addresses,
                        ..
                    } => {
                        addresses.iter().for_each(|addr| {
                            self.swarm.add_peer_address(peer, addr.clone());
                        });
                        self.swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer);
                        if let Some(old_peer) = old_peer {
                            self.swarm.behaviour_mut().gossipsub.remove_explicit_peer(&old_peer);
                            if let Err(error) = self.client_peer_changes_tx.send(()) {
                                error!(target: LOG_TARGET, "Failed to send peer changes trigger: {error:?}");
                            }
                        }
                    },
                    _ => debug!(target: LOG_TARGET, "[KADEMLIA] {event:?}"),
                },
            },
            _ => {},
        };
    }

    /// Main loop of the service that drives the events and libp2p swarm forward.
    async fn main_loop(&mut self) -> Result<(), Error> {
        let mut publish_peer_info_interval = tokio::time::interval(self.config.peer_info_publish_interval);

        loop {
            select! {
                event = self.swarm.select_next_some() => {
                   self.handle_event(event).await;
                }
                result = self.client_validate_block_req_rx.recv() => {
                    self.handle_client_validate_block_request(result).await;
                }
                block = self.client_broadcast_block_rx.recv() => {
                    self.broadcast_block(block).await;
                }
                _ = publish_peer_info_interval.tick() => {
                    // handle case when we have some peers removed
                    let expired_peers = self.peer_store.cleanup().await;
                    for exp_peer in expired_peers {
                        self.swarm.behaviour_mut().kademlia.remove_peer(&exp_peer);
                        self.swarm.behaviour_mut().gossipsub.remove_explicit_peer(&exp_peer);
                        if let Err(error) = self.client_peer_changes_tx.send(()) {
                            error!(target: LOG_TARGET, "Failed to send peer changes trigger: {error:?}");
                        }
                    }

                    // broadcast peer info
                    info!(target: LOG_TARGET, "Peer count: {:?}", self.peer_store.peer_count().await);
                    if let Err(error) = self.broadcast_peer_info().await {
                        match error {
                            Error::LibP2P(LibP2PError::Publish(PublishError::InsufficientPeers)) => {
                                warn!(target: LOG_TARGET, "No peers to broadcast peer info!");
                            }
                            Error::LibP2P(LibP2PError::Publish(PublishError::Duplicate)) => {}
                            _ => {
                                error!(target: LOG_TARGET, "Failed to publish node info: {error:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    fn join_seed_peers(&mut self) -> Result<(), Error> {
        if self.config.seed_peers.is_empty() {
            return Ok(());
        }

        for seed_peer in &self.config.seed_peers {
            let addr = seed_peer
                .parse::<Multiaddr>()
                .map_err(|error| Error::LibP2P(LibP2PError::MultiAddrParse(error)))?;
            let peer_id = match addr.iter().last() {
                Some(Protocol::P2p(peer_id)) => Some(peer_id),
                _ => None,
            };
            if peer_id.is_none() {
                return Err(Error::LibP2P(LibP2PError::MissingPeerId(seed_peer.clone())));
            }
            self.swarm.behaviour_mut().kademlia.add_address(&peer_id.unwrap(), addr);
        }

        self.swarm
            .behaviour_mut()
            .kademlia
            .bootstrap()
            .map_err(|error| Error::LibP2P(LibP2PError::KademliaNoKnownPeers(error)))?;

        Ok(())
    }

    /// Starts p2p service.
    /// Please note that this is a blocking call!
    pub async fn start(&mut self) -> Result<(), Error> {
        self.swarm
            .listen_on(
                format!("/ip4/0.0.0.0/tcp/{}", self.port)
                    .parse()
                    .map_err(|e| Error::LibP2P(LibP2PError::MultiAddrParse(e)))?,
            )
            .map_err(|e| Error::LibP2P(LibP2PError::Transport(e)))?;

        self.join_seed_peers()?;
        self.subscribe_to_topics();

        self.main_loop().await
    }
}

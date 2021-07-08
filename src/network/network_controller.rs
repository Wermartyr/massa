use super::config::NetworkConfig;
use super::peer_info_database::*;
use crate::logging::{debug, trace};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::net::{IpAddr, SocketAddr};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

type BoxResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Default, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectionId(u64);

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl std::fmt::Debug for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum ConnectionClosureReason {
    Normal,
    Failed,
    Banned,
}

#[derive(Debug)]
enum NetworkCommand {
    MergeAdvertisedPeerList(Vec<IpAddr>),
    GetAdvertisablePeerList(oneshot::Sender<Vec<IpAddr>>),
    ConnectionClosed((ConnectionId, ConnectionClosureReason)),
    ConnectionAlive(ConnectionId),
}

#[derive(Debug)]
pub enum NetworkEvent {
    NewConnection((ConnectionId, TcpStream)),
    ConnectionBanned(ConnectionId),
}

pub struct NetworkController {
    network_command_tx: mpsc::Sender<NetworkCommand>,
    network_event_rx: mpsc::Receiver<NetworkEvent>,
    controller_fn_handle: JoinHandle<()>,
}

impl NetworkController {
    /// Starts a new NetworkController from NetworkConfig
    /// can panic if :
    /// - config routable_ip IP is not routable
    pub async fn new(cfg: &NetworkConfig) -> BoxResult<Self> {
        debug!("starting network controller");
        massa_trace!("start", {});

        // check that local IP is routable
        if let Some(self_ip) = cfg.routable_ip {
            if !self_ip.is_global() {
                panic!("config routable_ip IP is not routable");
            }
        }

        // create listener
        let listener = TcpListener::bind(&cfg.bind).await?;

        // load peer info database
        let peer_info_db = PeerInfoDatabase::new(&cfg).await?;

        // launch controller
        let (network_command_tx, network_command_rx) = mpsc::channel::<NetworkCommand>(1024);
        let (network_event_tx, network_event_rx) = mpsc::channel::<NetworkEvent>(1024);
        let cfg_copy = cfg.clone();
        let controller_fn_handle = tokio::spawn(async move {
            network_controller_fn(
                cfg_copy,
                listener,
                peer_info_db,
                network_command_rx,
                network_event_tx,
            )
            .await;
        });

        debug!("network controller started");
        massa_trace!("ready", {});

        Ok(NetworkController {
            network_command_tx,
            network_event_rx,
            controller_fn_handle,
        })
    }

    /// Stops the NetworkController properly
    /// can panic if network controller is not reachable
    pub async fn stop(mut self) {
        debug!("stopping network controller");
        massa_trace!("begin", {});
        drop(self.network_command_tx);
        while let Some(_) = self.network_event_rx.next().await {}
        self.controller_fn_handle
            .await
            .expect("failed joining network controller");
        debug!("network controller stopped");
        massa_trace!("end", {});
    }

    pub async fn wait_event(&mut self) -> NetworkEvent {
        self.network_event_rx
            .recv()
            .await
            .expect("failed retrieving network controller event")
    }

    pub async fn merge_advertised_peer_list(&mut self, ips: Vec<IpAddr>) {
        self.network_command_tx
            .send(NetworkCommand::MergeAdvertisedPeerList(ips))
            .await
            .expect("network controller disappeared");
    }

    pub async fn get_advertisable_peer_list(&mut self) -> Vec<IpAddr> {
        let (response_tx, response_rx) = oneshot::channel::<Vec<IpAddr>>();
        self.network_command_tx
            .send(NetworkCommand::GetAdvertisablePeerList(response_tx))
            .await
            .expect("network controller disappeared");
        response_rx.await.expect("network controller disappeared")
    }

    pub async fn connection_closed(&mut self, id: ConnectionId, reason: ConnectionClosureReason) {
        self.network_command_tx
            .send(NetworkCommand::ConnectionClosed((id, reason)))
            .await
            .expect("network controller disappeared");
    }

    pub async fn connection_alive(&mut self, id: ConnectionId) {
        self.network_command_tx
            .send(NetworkCommand::ConnectionAlive(id))
            .await
            .expect("network controller disappeared");
    }
}

async fn network_controller_fn(
    cfg: NetworkConfig,
    listener: TcpListener,
    mut peer_info_db: PeerInfoDatabase,
    mut network_command_rx: mpsc::Receiver<NetworkCommand>,
    event_tx: mpsc::Sender<NetworkEvent>,
) {
    let mut out_connecting_futures = FuturesUnordered::new();
    let mut cur_connection_id = ConnectionId::default();
    let mut active_connections: HashMap<ConnectionId, (IpAddr, bool)> = HashMap::new(); // ip, is_outgoing
    let mut wakeup_interval = tokio::time::interval(cfg.wakeup_interval); // wake up the controller at a regular interval to retry connections

    loop {
        {
            // try to connect to candidate IPs
            let candidate_ips = peer_info_db.get_out_connection_candidate_ips();
            for ip in candidate_ips {
                debug!("starting outgoing connection attempt towards ip={:?}", ip);
                massa_trace!("out_connection_attempt_start", { "ip": ip });
                peer_info_db.new_out_connection_attempt(&ip);
                out_connecting_futures.push(out_connector_fn(
                    SocketAddr::new(ip, cfg.protocol_port),
                    cfg.connect_timeout,
                ));
            }
        }

        tokio::select! {
            // wake up interval
            _ = wakeup_interval.tick() => {},

            // peer feedback event
            res = network_command_rx.next() => match res {
                Some(cmd) => manage_network_command(
                    cmd,
                    &mut peer_info_db,
                    &mut active_connections,
                    &event_tx
                ).await,
                None => break,
            },
            // out-connector event
            Some((ip_addr, res)) = out_connecting_futures.next() => manage_out_connections(
                res,
                ip_addr,
                &mut peer_info_db,
                &mut cur_connection_id,
                &mut active_connections,
                &event_tx,
            ).await,

            // listener socket received
            res = listener.accept() => manage_in_connections(res,
                &mut peer_info_db,
                &mut cur_connection_id,
                &mut active_connections,
                &event_tx
            ).await,
        }
    }

    // wait for out-connectors to finish
    while let Some(_) = out_connecting_futures.next().await {}

    // stop peer info db
    peer_info_db.stop().await;
}

async fn out_connector_fn(
    addr: SocketAddr,
    timeout_duration: Duration,
) -> (IpAddr, Result<TcpStream, String>) {
    match timeout(timeout_duration, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(sock)) => (addr.ip(), Ok(sock)),
        Ok(Err(e)) => (addr.ip(), Err(format!("TCP connection failed: {:?}", e))),
        Err(_) => (addr.ip(), Err(format!("TCP connection timed out"))),
    }
}

async fn manage_network_command(
    cmd: NetworkCommand,
    peer_info_db: &mut PeerInfoDatabase,
    active_connections: &mut HashMap<ConnectionId, (IpAddr, bool)>,
    event_tx: &mpsc::Sender<NetworkEvent>,
) {
    match cmd {
        NetworkCommand::MergeAdvertisedPeerList(ips) => {
            debug!("merging incoming peer list: {:?}", ips);
            massa_trace!("merge_incoming_peer_list", { "ips": ips });
            peer_info_db.merge_candidate_peers(&ips);
        }
        NetworkCommand::GetAdvertisablePeerList(response_tx) => {
            response_tx
                .send(peer_info_db.get_advertisable_peer_ips())
                .expect("upstream disappeared");
        }
        NetworkCommand::ConnectionClosed((id, reason)) => {
            let (ip, is_outgoing) = active_connections
                .remove(&id)
                .expect("missing connection closed");
            debug!(
                "connection closed connedtion_id={:?}, ip={:?}, reason={:?}",
                id, ip, reason
            );
            massa_trace!("connection_closed", {
                "connnection_id": id,
                "ip": ip,
                "reason": reason
            });
            match reason {
                ConnectionClosureReason::Normal => {}
                ConnectionClosureReason::Failed => {
                    peer_info_db.peer_failed(&ip);
                }
                ConnectionClosureReason::Banned => {
                    peer_info_db.peer_banned(&ip);
                    // notify all other banned connections to close
                    // note: they must close using Normal reason or else risk of feedback loop
                    let target_ids =
                        active_connections
                            .iter()
                            .filter_map(|(other_id, (other_ip, _))| {
                                if *other_ip == ip {
                                    Some(other_id)
                                } else {
                                    None
                                }
                            });
                    for target_id in target_ids {
                        event_tx
                            .send(NetworkEvent::ConnectionBanned(*target_id))
                            .await
                            .expect("could not send connection banned notification upstream");
                    }
                }
            }
            if is_outgoing {
                peer_info_db.out_connection_closed(&ip);
            } else {
                peer_info_db.in_connection_closed(&ip);
            }
        }
        NetworkCommand::ConnectionAlive(id) => {
            let (ip, _) = active_connections
                .get(&id)
                .expect("missing connection alive");
            peer_info_db.peer_alive(&ip);
        }
    }
}

async fn manage_out_connections(
    res: Result<TcpStream, String>,
    ip_addr: IpAddr,
    peer_info_db: &mut PeerInfoDatabase,
    cur_connection_id: &mut ConnectionId,
    active_connections: &mut HashMap<ConnectionId, (IpAddr, bool)>,
    event_tx: &mpsc::Sender<NetworkEvent>,
) {
    match res {
        Ok(socket) => {
            if peer_info_db.try_out_connection_attempt_success(&ip_addr) {
                // outgoing connection established
                let connection_id = *cur_connection_id;
                debug!(
                    "out connection towards ip={:?} established => connection_id={:?}",
                    ip_addr, connection_id
                );
                massa_trace!("out_connection_established", {
                    "ip": ip_addr,
                    "connection_id": connection_id
                });
                cur_connection_id.0 += 1;
                active_connections.insert(connection_id, (ip_addr, true));
                event_tx
                    .send(NetworkEvent::NewConnection((connection_id, socket)))
                    .await
                    .expect("could not send new out connection notification");
            } else {
                debug!("out connection towards ip={:?} refused", ip_addr);
                massa_trace!("out_connection_refused", { "ip": ip_addr });
            }
        }
        Err(err) => {
            debug!(
                "outgoing connection attempt towards ip={:?} failed: {:?}",
                ip_addr, err
            );
            massa_trace!("out_connection_attempt_failed", {
                "ip": ip_addr,
                "err": err.to_string()
            });
            peer_info_db.out_connection_attempt_failed(&ip_addr);
        }
    }
}

async fn manage_in_connections(
    res: std::io::Result<(TcpStream, SocketAddr)>,
    peer_info_db: &mut PeerInfoDatabase,
    cur_connection_id: &mut ConnectionId,
    active_connections: &mut HashMap<ConnectionId, (IpAddr, bool)>,
    event_tx: &mpsc::Sender<NetworkEvent>,
) {
    match res {
        Ok((socket, remote_addr)) => {
            if peer_info_db.try_new_in_connection(&remote_addr.ip()) {
                let connection_id = *cur_connection_id;
                debug!(
                    "inbound connection from addr={:?} succeeded => connection_id={:?}",
                    remote_addr, connection_id
                );
                massa_trace!("in_connection_established", {
                    "ip": remote_addr.ip(),
                    "connection_id": connection_id
                });
                cur_connection_id.0 += 1;
                active_connections.insert(connection_id, (remote_addr.ip(), false));
                event_tx
                    .send(NetworkEvent::NewConnection((connection_id, socket)))
                    .await
                    .expect("could not send new in connection notification");
            } else {
                debug!("inbound connection from addr={:?} refused", remote_addr);
                massa_trace!("in_connection_refused", {"ip": remote_addr.ip()});
            }
        }
        Err(err) => {
            debug!("connection accept failed: {:?}", err);
            massa_trace!("in_connection_failed", {"err": err.to_string()});
        }
    }
}

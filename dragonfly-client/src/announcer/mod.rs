/*
 *     Copyright 2023 The Dragonfly Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::grpc::{manager::ManagerClient, scheduler::SchedulerClient, REQUEST_TIMEOUT};
use crate::shutdown;
use dragonfly_api::common::v2::{Build, Cpu, Disk, Host, Memory, Network, Peer, Piece, Task};
use dragonfly_api::manager::v2::{DeleteSeedPeerRequest, SourceType, UpdateSeedPeerRequest};
use dragonfly_api::scheduler::v2::{
    AnnounceHostRequest, AnnouncePeersRequest, DeleteHostRequest, DeleteTaskRequest,
};
use dragonfly_client_config::{
    dfdaemon::{Config, HostType},
    CARGO_PKG_RUSTC_VERSION, CARGO_PKG_VERSION, GIT_HASH,
};
use dragonfly_client_core::error::{ErrorType, OrErr};
use dragonfly_client_core::{Error, Result};
use dragonfly_client_storage::Storage;
use dragonfly_client_util::id_generator::IDGenerator;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use sysinfo::System;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tracing::{error, info, Instrument};

// ManagerAnnouncer is used to announce the dfdaemon information to the manager.
pub struct ManagerAnnouncer {
    // config is the configuration of the dfdaemon.
    config: Arc<Config>,

    // manager_client is the grpc client of the manager.
    manager_client: Arc<ManagerClient>,

    // shutdown is used to shutdown the announcer.
    shutdown: shutdown::Shutdown,

    // _shutdown_complete is used to notify the announcer is shutdown.
    _shutdown_complete: mpsc::UnboundedSender<()>,
}

// ManagerAnnouncer implements the manager announcer of the dfdaemon.
impl ManagerAnnouncer {
    // new creates a new manager announcer.
    pub fn new(
        config: Arc<Config>,
        manager_client: Arc<ManagerClient>,
        shutdown: shutdown::Shutdown,
        shutdown_complete_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        Self {
            config,
            manager_client,
            shutdown,
            _shutdown_complete: shutdown_complete_tx,
        }
    }

    // run announces the dfdaemon information to the manager.
    pub async fn run(&self) -> Result<()> {
        // Clone the shutdown channel.
        let mut shutdown = self.shutdown.clone();

        // If the seed peer is enabled, we should announce the seed peer to the manager.
        if self.config.seed_peer.enable {
            // Register the seed peer to the manager.
            self.manager_client
                .update_seed_peer(UpdateSeedPeerRequest {
                    source_type: SourceType::SeedPeerSource.into(),
                    hostname: self.config.host.hostname.clone(),
                    r#type: self.config.seed_peer.kind.to_string(),
                    idc: self.config.host.idc.clone(),
                    location: self.config.host.location.clone(),
                    ip: self.config.host.ip.unwrap().to_string(),
                    port: self.config.upload.server.port as i32,
                    download_port: self.config.upload.server.port as i32,
                    seed_peer_cluster_id: self.config.seed_peer.cluster_id,
                })
                .await?;

            // Announce to scheduler shutting down with signals.
            shutdown.recv().await;

            // Delete the seed peer from the manager.
            self.manager_client
                .delete_seed_peer(DeleteSeedPeerRequest {
                    source_type: SourceType::SeedPeerSource.into(),
                    hostname: self.config.host.hostname.clone(),
                    ip: self.config.host.ip.unwrap().to_string(),
                    seed_peer_cluster_id: self.config.seed_peer.cluster_id,
                })
                .await?;

            info!("announce to manager shutting down");
        } else {
            shutdown.recv().await;
            info!("announce to manager shutting down");
        }

        Ok(())
    }
}

// Announcer is used to announce the dfdaemon information to the manager and scheduler.
pub struct SchedulerAnnouncer {
    // config is the configuration of the dfdaemon.
    config: Arc<Config>,

    // host_id is the id of the host.
    host_id: String,

    // scheduler_client is the grpc client of the scheduler.
    scheduler_client: Arc<SchedulerClient>,

    // shutdown is used to shutdown the announcer.
    shutdown: shutdown::Shutdown,

    // _shutdown_complete is used to notify the announcer is shutdown.
    _shutdown_complete: mpsc::UnboundedSender<()>,
}

// SchedulerAnnouncer implements the scheduler announcer of the dfdaemon.
impl SchedulerAnnouncer {
    // new creates a new scheduler announcer.
    pub async fn new(
        config: Arc<Config>,
        scheduler_client: Arc<SchedulerClient>,
        shutdown: shutdown::Shutdown,
        shutdown_complete_tx: mpsc::UnboundedSender<()>,
        id_generator: Arc<IDGenerator>,
        storage: Arc<Storage>,
    ) -> Result<Self> {
        let announcer = Self {
            config,
            host_id: id_generator.host_id(),
            scheduler_client,
            shutdown,
            _shutdown_complete: shutdown_complete_tx,
        };

        // Initialize the scheduler announcer.
        announcer
            .scheduler_client
            .init_announce_host(announcer.make_announce_host_request()?)
            .await?;

        // Announce peers to the scheduler after host announcement.
        announcer
            .announce_peers(id_generator.clone(), storage.clone())
            .await?;

        Ok(announcer)
    }

    // run announces the dfdaemon information to the scheduler.
    pub async fn run(&self) {
        // Clone the shutdown channel.
        let mut shutdown = self.shutdown.clone();

        // Start the scheduler announcer.
        let mut interval = tokio::time::interval(self.config.scheduler.announce_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let request = match self.make_announce_host_request() {
                        Ok(request) => request,
                        Err(err) => {
                            error!("make announce host request failed: {}", err);
                            continue;
                        }
                    };

                    if let Err(err) = self.scheduler_client.announce_host(request).await {
                        error!("announce host to scheduler failed: {}", err);
                    };
                }
                _ = shutdown.recv() => {
                    // Announce to scheduler shutting down with signals.
                    if let Err(err) = self.scheduler_client.delete_host(DeleteHostRequest{
                        host_id: self.host_id.clone(),
                    }).await {
                        error!("delete host from scheduler failed: {}", err);
                    }

                    info!("announce to scheduler shutting down");
                    return
                }
            }
        }
    }

    // make_announce_host_request makes the announce host request.
    fn make_announce_host_request(&self) -> Result<AnnounceHostRequest> {
        // If the seed peer is enabled, we should announce the seed peer to the scheduler.
        let host_type = if self.config.seed_peer.enable {
            self.config.seed_peer.kind
        } else {
            HostType::Normal
        };

        // Get the system information.
        let mut sys = System::new_all();
        sys.refresh_all();

        // Get the process information.
        let process = sys.process(sysinfo::get_current_pid().unwrap()).unwrap();

        // Get the cpu information.
        let cpu = Cpu {
            logical_count: sys.physical_core_count().unwrap_or_default() as u32,
            physical_count: sys.physical_core_count().unwrap_or_default() as u32,
            percent: sys.global_cpu_info().cpu_usage() as f64,
            process_percent: process.cpu_usage() as f64,

            // TODO: Get the cpu times.
            times: None,
        };

        // Get the memory information.
        let memory = Memory {
            total: sys.total_memory(),
            available: sys.available_memory(),
            used: sys.used_memory(),
            used_percent: (sys.used_memory() / sys.total_memory()) as f64,
            process_used_percent: (process.memory() / sys.total_memory()) as f64,
            free: sys.free_memory(),
        };

        // Get the network information.
        let network = Network {
            // TODO: Get the count of the tcp connection.
            tcp_connection_count: 0,

            // TODO: Get the count of the upload tcp connection.
            upload_tcp_connection_count: 0,
            idc: self.config.host.idc.clone(),
            location: self.config.host.location.clone(),
        };

        // Get the disk information.
        let stats = fs2::statvfs(self.config.storage.dir.as_path())?;
        let total_space = stats.total_space();
        let available_space = stats.available_space();
        let used_space = total_space - available_space;
        let used_percent = (used_space as f64 / (total_space) as f64) * 100.0;

        let disk = Disk {
            total: total_space,
            free: available_space,
            used: used_space,
            used_percent,

            // TODO: Get the disk inodes information.
            inodes_total: 0,
            inodes_used: 0,
            inodes_free: 0,
            inodes_used_percent: 0.0,
        };

        // Get the build information.
        let build = Build {
            git_version: CARGO_PKG_VERSION.to_string(),
            git_commit: Some(GIT_HASH.unwrap_or_default().to_string()),
            go_version: None,
            rust_version: Some(CARGO_PKG_RUSTC_VERSION.to_string()),
            platform: None,
        };

        // Struct the host information.
        let host = Host {
            id: self.host_id.to_string(),
            r#type: host_type as u32,
            hostname: self.config.host.hostname.clone(),
            ip: self.config.host.ip.unwrap().to_string(),
            port: self.config.upload.server.port as i32,
            download_port: self.config.upload.server.port as i32,
            os: env::consts::OS.to_string(),
            platform: env::consts::OS.to_string(),
            platform_family: env::consts::FAMILY.to_string(),
            platform_version: System::os_version().unwrap_or_default(),
            kernel_version: System::kernel_version().unwrap_or_default(),
            cpu: Some(cpu),
            memory: Some(memory),
            network: Some(network),
            disk: Some(disk),
            build: Some(build),

            // TODO: Get scheduler cluster id from dynconfig.
            scheduler_cluster_id: 0,
        };

        Ok(AnnounceHostRequest {
            host: Some(host),
            interval: Some(
                prost_wkt_types::Duration::try_from(self.config.scheduler.announce_interval)
                    .or_err(ErrorType::ParseError)?,
            ),
        })
    }

    // announce_peers announces peers to scheduler after host announcement.
    async fn announce_peers(
        &self,
        id_generator: Arc<IDGenerator>,
        storage: Arc<Storage>,
    ) -> Result<()> {
        // Announce peers with a maximum concurrency of 5.
        let mut join_set = JoinSet::new();
        let semaphore = Arc::new(Semaphore::new(5));

        for request in self
            .make_announce_peers_request(
                id_generator.clone(),
                storage.clone(),
                self.scheduler_client.clone(),
            )
            .await?
        {
            // Get the task_id to select scheduler for the announce peers request.
            let task_id = if let Some(task) = request.peers[0].task.as_ref() {
                task.id.clone()
            } else {
                continue;
            };

            async fn announce_peers(
                scheduler_client: Arc<SchedulerClient>,
                task_id: String,
                request: AnnouncePeersRequest,
                semaphore: Arc<Semaphore>,
            ) -> Result<()> {
                // Limit the concurrent announcement count.
                let _permit = semaphore.acquire().await.unwrap();

                // Initialize stream channel.
                let (in_stream_tx, in_stream_rx) = mpsc::channel(4096);

                // Initialize the stream.
                let in_stream = ReceiverStream::new(in_stream_rx);
                let request_stream = Request::new(in_stream);

                // Announce peers to the scheduler.
                scheduler_client
                    .announce_peers(task_id.as_str(), request_stream)
                    .await?;

                // Send the announce peers request, in groups of 10.
                for chunk in request.peers.chunks(10) {
                    in_stream_tx
                        .send_timeout(
                            AnnouncePeersRequest {
                                peers: chunk.to_vec(),
                            },
                            REQUEST_TIMEOUT,
                        )
                        .await
                        .map_err(|err| {
                            error!("send AnnouncePeersRequest failed: {:?}", err);
                            err
                        })?;
                }

                Ok(())
            }

            join_set.spawn(
                announce_peers(
                    self.scheduler_client.clone(),
                    task_id,
                    request,
                    semaphore.clone(),
                )
                .in_current_span(),
            );
        }

        while let Some(message) = join_set
            .join_next()
            .await
            .transpose()
            .or_err(ErrorType::AsyncRuntimeError)?
        {
            match message {
                Err(Error::SendTimeout) => {
                    join_set.detach_all();
                }
                Err(err) => {
                    error!("failed to announce peers: {}", err);
                }
                Ok(_) => {}
            }
        }

        Ok(())
    }

    // make_announce_peers_request makes the announce peers request.
    async fn make_announce_peers_request(
        &self,
        id_generator: Arc<IDGenerator>,
        storage: Arc<Storage>,
        scheduler_client: Arc<SchedulerClient>,
    ) -> Result<Vec<AnnouncePeersRequest>> {
        // Get the hash ring.
        let addrs = scheduler_client.hashring.read().await;

        let mut peers = HashMap::new();

        for task in storage.get_tasks()? {
            // If the task is expired or not finished, evict the task in scheduler.
            if task.is_expired(self.config.gc.policy.task_ttl) || !task.is_finished() {
                scheduler_client
                    .delete_task(DeleteTaskRequest {
                        host_id: self.host_id.clone(),
                        task_id: task.id.clone(),
                    })
                    .await
                    .unwrap_or_else(|err| {
                        error!("failed to delete peer {}: {}", task.id, err);
                    });
                continue;
            }

            // Get the pieces of a peer based on the task metadata from the local storage.
            let mut pieces = vec![];
            for piece in storage.get_pieces(task.id.as_str()).unwrap_or_default() {
                pieces.push(Piece {
                    number: piece.number,
                    parent_id: piece.parent_id,
                    offset: piece.offset,
                    length: piece.length,
                    digest: piece.digest,
                    ..Default::default()
                });
            }

            // Construct a peer based on the task metadata from the local storage.
            let peer = Peer {
                id: id_generator.peer_id(),
                pieces,
                task: Some(Task {
                    id: task.id.clone(),
                    piece_length: task.piece_length,
                    content_length: task.content_length.unwrap_or_default(),
                    ..Default::default()
                }),
                host: Some(Host {
                    id: self.host_id.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            };

            // Get the scheduler address from the hash ring.
            let addr = *addrs
                .get(&task.id[0..5].to_string())
                .ok_or_else(|| Error::HashRing(task.id.to_string()))?;

            peers.entry(addr.to_string()).or_insert(vec![]).push(peer);
        }

        let mut requests: Vec<AnnouncePeersRequest> = Vec::new();
        for (_, peers) in peers {
            requests.push(AnnouncePeersRequest { peers });
        }

        Ok(requests)
    }
}

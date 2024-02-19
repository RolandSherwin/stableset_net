// Copyright (C) 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[macro_use]
extern crate tracing;

use clap::Parser;
use color_eyre::{self, eyre::Result};
use libp2p::Multiaddr;
use sn_node_manager::{
    config::get_node_registry_path, daemon_control, node_control, service::NodeServiceManager,
};
use sn_node_rpc_client::{RpcActions, RpcClient};
use sn_protocol::{
    node_registry::{Node, NodeRegistry, NodeStatus},
    safenode_manager_proto::{
        safe_node_manager_server::{SafeNodeManager, SafeNodeManagerServer},
        RestartRequest, RestartResponse,
    },
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::sync::{
    mpsc::{self, Sender},
    oneshot,
};
use tonic::{transport::Server, Code, Request, Response, Status};

const PORT: u16 = 12500;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Specify a port for the daemon to listen for RPCs. It defaults to 12500 if not set.
    #[clap(long, default_value_t = PORT)]
    port: u16,
    /// Specify an Ipv4Addr for the daemon to listen on. This is useful if you want to manage the nodes remotely.
    ///
    /// If not set, the daemon listens locally for commands.
    #[clap(long, default_value_t = Ipv4Addr::new(127, 0, 0, 1))]
    address: Ipv4Addr,
}

struct SafeNodeManagerDaemon {
    cmd_sender: Sender<Commands>,
}

// Implementing RPC interface for service defined in .proto
#[tonic::async_trait]
impl SafeNodeManager for SafeNodeManagerDaemon {
    async fn restart(
        &self,
        request: Request<RestartRequest>,
    ) -> Result<Response<RestartResponse>, Status> {
        println!("RPC request received {:?}", request.get_ref());
        info!("RPC request received {:?}", request.get_ref());

        Self::restart_handler().await.map_err(|err| {
            Status::new(Code::Internal, format!("Failed to stop the node: {err}"))
        })?;

        // let (tx, rx) = oneshot::channel();
        // self.cmd_sender
        //     .send(Commands::Restart { response: tx })
        //     .await
        //     .map_err(|err| Status::new(Code::Internal, format!("Error inside the actor {err}")))?;
        // rx.await
        //     .map_err(|err| Status::new(Code::Internal, format!("Error inside the actor {err}")))?
        //     .map_err(|err| {
        //         Status::new(Code::Internal, format!("Failed to stop the node: {err}"))
        //     })?;

        Ok(Response::new(RestartResponse {}))
    }
}

impl SafeNodeManagerDaemon {
    async fn restart_handler() -> Result<()> {
        let mut node_registry = NodeRegistry::load(&get_node_registry_path()?)?;

        let mut node = &mut node_registry.nodes[0];
        let rpc_client = RpcClient::from_socket_addr(node.rpc_socket_addr);

        // Self::gg(
        //     &mut node,
        //     rpc_client,
        //     node_registry.bootstrap_peers,
        //     node_registry.environment_variables,
        //     NodeServiceManager {},
        // )
        // .await;

        daemon_control::restart_safenode(
            &mut node,
            &rpc_client,
            node_registry.bootstrap_peers,
            node_registry.environment_variables,
            &NodeServiceManager {},
        )
        .await?;

        Ok(())
    }
}

// The SafeNodeManager trait returns `Status` as its error. So the actual logic is here and we can easily map the errors
// into Status inside the trait fns.
impl SafeNodeManagerDaemon {}

enum Commands {
    Restart {
        response: oneshot::Sender<Result<()>>,
    },
}

struct Actor {}

impl Actor {
    pub fn spawn_actor() -> Sender<Commands> {
        let (tx, mut rx) = mpsc::channel(100);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    Commands::Restart { response } => {
                        if let Err(err) = response.send(Self::restart_handler().await) {
                            println!("Error while sending response to caller: {err:?}");
                            error!("Error while sending response to caller: {err:?}");
                        }
                    }
                }
            }
        });
        tx
    }

    async fn restart_handler() -> Result<()> {
        let mut node_registry = NodeRegistry::load(&get_node_registry_path()?)?;

        let mut node = &mut node_registry.nodes[0];
        let rpc_client = RpcClient::from_socket_addr(node.rpc_socket_addr);

        // Self::gg(
        //     &mut node,
        //     rpc_client,
        //     node_registry.bootstrap_peers,
        //     node_registry.environment_variables,
        //     NodeServiceManager {},
        // )
        // .await;

        daemon_control::restart_safenode(
            node,
            &rpc_client,
            node_registry.bootstrap_peers,
            node_registry.environment_variables,
            &NodeServiceManager {},
        )
        .await?;

        Ok(())
    }

    // async fn gg(
    //     node: &mut Node,
    //     rpc_client: RpcClient,
    //     bootstrap_peers: Vec<Multiaddr>,
    //     env_variables: Option<Vec<(String, String)>>,
    //     service_control: NodeServiceManager,
    // ) {
    //     // node_control::stop(node, &service_control).await;
    //     match node.status {
    //         NodeStatus::Added => {
    //             println!(
    //                 "Service {} has not been started since it was installed",
    //                 node.service_name
    //             );
    //             // Ok(())
    //         }
    //         _ => node.status = NodeStatus::Removed,
    //     }
    // }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    println!("Starting safenode-manager-daemon");
    let args = Args::parse();
    let cmd_sender = Actor::spawn_actor();
    let service = SafeNodeManagerDaemon { cmd_sender };

    // adding our service to our server.
    if let Err(err) = Server::builder()
        .add_service(SafeNodeManagerServer::new(service))
        .serve(SocketAddr::new(IpAddr::V4(args.address), args.port))
        .await
    {
        error!("Safenode Manager Daemon failed to start: {err:?}");
        println!("Safenode Manager Daemon failed to start: {err:?}");
        return Err(err.into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::PORT;
    use color_eyre::eyre::{bail, Result};
    use sn_protocol::safenode_manager_proto::{
        safe_node_manager_client::SafeNodeManagerClient, RestartRequest,
    };
    use std::{
        net::{Ipv4Addr, SocketAddr},
        time::Duration,
    };
    use tonic::Request;

    #[tokio::test]
    async fn restart() -> Result<()> {
        let mut rpc_client = get_safenode_manager_rpc_client(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            PORT,
        ))
        .await?;

        let response = rpc_client
            .restart(Request::new(RestartRequest { delay_millis: 0 }))
            .await?;
        println!("response: {response:?}");

        Ok(())
    }

    // Connect to a RPC socket addr with retry
    pub async fn get_safenode_manager_rpc_client(
        socket_addr: SocketAddr,
    ) -> Result<SafeNodeManagerClient<tonic::transport::Channel>> {
        // get the new PeerId for the current NodeIndex
        let endpoint = format!("https://{socket_addr}");
        let mut attempts = 0;
        loop {
            if let Ok(rpc_client) = SafeNodeManagerClient::connect(endpoint.clone()).await {
                break Ok(rpc_client);
            }
            attempts += 1;
            println!("Could not connect to rpc {endpoint:?}. Attempts: {attempts:?}/10");
            error!("Could not connect to rpc {endpoint:?}. Attempts: {attempts:?}/10");
            tokio::time::sleep(Duration::from_secs(1)).await;
            if attempts >= 10 {
                bail!("Failed to connect to {endpoint:?} even after 10 retries");
            }
        }
    }
}

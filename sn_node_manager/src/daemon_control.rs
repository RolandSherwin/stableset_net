// Copyright (C) 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{node_control, service::ServiceControl, VerbosityLevel};
use color_eyre::{
    eyre::{eyre, OptionExt},
    Result,
};
use libp2p::Multiaddr;
use service_manager::{ServiceInstallCtx, ServiceLabel};
use sn_node_rpc_client::RpcActions;
use sn_protocol::{node::get_safenode_root_dir, node_registry::Node};
use std::{
    ffi::OsString,
    net::Ipv4Addr,
    path::PathBuf,
    process::{Command, Stdio},
};
use sysinfo::{Pid, PidExt, ProcessExt, SystemExt};

pub fn run(
    address: Ipv4Addr,
    port: u16,
    daemon_path: PathBuf,
    service_control: &dyn ServiceControl,
    _verbosity: VerbosityLevel,
) -> Result<()> {
    let service_name: ServiceLabel = "safenode-manager-daemon".parse()?;

    let install_ctx = ServiceInstallCtx {
        label: service_name.clone(),
        program: daemon_path,
        args: vec![
            OsString::from("--port"),
            OsString::from(port.to_string()),
            OsString::from("--address"),
            OsString::from(address.to_string()),
        ],
        contents: None,
        username: None,
        working_directory: None,
        environment: None,
    };
    service_control.install(install_ctx)?;
    service_control.start(&service_name.to_string())?;

    Ok(())
}

pub async fn restart_safenode_same_peer_id(
    node: &mut Node,
    rpc_client: &dyn RpcActions,
    bootstrap_peers: Vec<Multiaddr>,
    _env_variables: Option<Vec<(String, String)>>,
    service_control: &dyn ServiceControl,
) -> Result<()> {
    // match restart_kind {
    //     RestartKind::LocalNode => {
    //         let pid = node.pid.ok_or_eyre("Could not find node's PeerId")?;
    //         match sysinfo::System::new_all().process(Pid::from_u32(pid)) {
    //             Some(process) => {
    //                 process.kill();
    //                 println!("Process with PID {} has been killed.", pid);
    //             }
    //             None => return Err(eyre!("Process with PID {} not found.", pid).into()),
    //         }

    //         let root_dir =
    //             get_safenode_root_dir(node.peer_id.ok_or_eyre("PeerId is present at this point")?)?;
    //         let node_port = node
    //             .get_safenode_port()
    //             .ok_or_eyre("Could not obtain node port")?;

    //         // todo: deduplicate code inside local.rs
    //         let mut args = Vec::new();
    //         for peer in bootstrap_peers {
    //             args.push("--peer".to_string());
    //             args.push(peer.to_string());
    //         }

    //         args.push("--local".to_string());
    //         args.push("--rpc".to_string());
    //         args.push(node.rpc_socket_addr.to_string());
    //         args.push("--root_dir".to_string());
    //         args.push(format!("{root_dir:?}"));
    //         args.push("--port".to_string());
    //         args.push(node_port.to_string());

    //         Command::new(&node.safenode_path)
    //             .args(args)
    //             .stdout(Stdio::inherit())
    //             .stderr(Stdio::inherit())
    //             .spawn()?;
    //     }
    //     RestartKind::ServiceNode => {
    //         node_control::stop(node, service_control).await?;

    //         // service_control.uninstall(&node.service_name.clone())?;
    //         // let install_ctx = node_control::InstallNodeServiceCtxBuilder {
    //         //     local: node.local,
    //         //     data_dir_path: node.data_dir_path.clone(),
    //         //     genesis: node.genesis,
    //         //     name: node.service_name.clone(),
    //         //     node_port: node.get_safenode_port(),
    //         //     bootstrap_peers,
    //         //     rpc_socket_addr: node.rpc_socket_addr,
    //         //     log_dir_path: node.log_dir_path.clone(),
    //         //     safenode_path: node.safenode_path.clone(),
    //         //     service_user: node.user.clone(),
    //         //     env_variables,
    //         // }
    //         // .execute()?;
    //         // service_control.install(install_ctx)?;

    //         node_control::start(node, service_control, rpc_client, VerbosityLevel::Normal).await?;
    //     }
    // }

    Ok(())
}

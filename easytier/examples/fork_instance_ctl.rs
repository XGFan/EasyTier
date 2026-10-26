//! Fork tooling, not part of upstream: add or delete a network instance on a
//! running easytier-core through its RPC portal, which easytier-cli cannot do.
//! `fork/mini.sh gate` uses it to join a second instance at runtime to the
//! shared virtual NIC the full-tunnel instance is on.
//!
//!   fork_instance_ctl [--rpc HOST:PORT] run <config.toml> [--wait SECS]
//!   fork_instance_ctl [--rpc HOST:PORT] delete <instance-id>
//!
//! `run` starts the instance, then watches it for up to `--wait` seconds
//! (default 20): it prints `failed <id>: <error>` and exits 3 as soon as the
//! core reports a start error, or `running <id>` and exits 0 if none appears.
//! Usage and RPC errors exit 2.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use easytier::common::config::{
    ConfigLoader as _, ConfigSource, NetworkConfig, NetworkConfigExt as _, TomlConfigLoader,
    config_source_to_rpc,
};
use easytier::proto::api::manage::{
    CollectNetworkInfoRequest, DeleteNetworkInstanceRequest, RunNetworkInstanceRequest,
    WebClientService, WebClientServiceClientFactory,
};
use easytier::proto::rpc::standalone::{RuntimeRpcClient, runtime_rpc_client};
use easytier::proto::rpc_types::controller::BaseController;
use uuid::Uuid;

type Manage = Box<dyn WebClientService<Controller = BaseController> + Send + Sync>;

const USAGE: &str = "usage: fork_instance_ctl [--rpc HOST:PORT] run <config.toml> [--wait SECS]\n       fork_instance_ctl [--rpc HOST:PORT] delete <instance-id>";

async fn manage(client: &mut RuntimeRpcClient) -> Result<Manage, String> {
    client
        .scoped_client::<WebClientServiceClientFactory<BaseController>>(String::new())
        .await
        .map_err(|e| format!("connect rpc portal: {e}"))
}

async fn run(
    client: &mut RuntimeRpcClient,
    path: &str,
    wait: Duration,
) -> Result<ExitCode, String> {
    let toml = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let loader = TomlConfigLoader::new_from_str(&toml).map_err(|e| format!("parse {path}: {e}"))?;
    let inst_id = loader.get_id();
    let config =
        NetworkConfig::new_from_config(&loader).map_err(|e| format!("convert {path}: {e}"))?;

    let manage = manage(client).await?;
    manage
        .run_network_instance(
            BaseController::default(),
            RunNetworkInstanceRequest {
                inst_id: Some(inst_id.into()),
                config: Some(config),
                overwrite: true,
                source: config_source_to_rpc(ConfigSource::User),
            },
        )
        .await
        .map_err(|e| format!("run network instance: {e}"))?;

    // starting is asynchronous: a rejected NIC claim only shows up later as
    // the instance's error message
    let deadline = Instant::now() + wait;
    loop {
        let info = manage
            .collect_network_info(
                BaseController::default(),
                CollectNetworkInfoRequest {
                    inst_ids: vec![inst_id.into()],
                },
            )
            .await
            .map_err(|e| format!("collect network info: {e}"))?;
        let state = info
            .info
            .and_then(|m| m.map.get(&inst_id.to_string()).cloned());
        if let Some(err) = state.as_ref().and_then(|s| s.error_msg.clone()) {
            println!("failed {inst_id}: {err}");
            return Ok(ExitCode::from(3));
        }
        if Instant::now() >= deadline {
            match state {
                Some(_) => {
                    println!("running {inst_id}");
                    return Ok(ExitCode::SUCCESS);
                }
                None => return Err(format!("instance {inst_id} not reported by the core")),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn delete(client: &mut RuntimeRpcClient, id: &str) -> Result<ExitCode, String> {
    let inst_id: Uuid = id
        .parse()
        .map_err(|e| format!("bad instance id {id}: {e}"))?;
    manage(client)
        .await?
        .delete_network_instance(
            BaseController::default(),
            DeleteNetworkInstanceRequest {
                inst_ids: vec![inst_id.into()],
            },
        )
        .await
        .map_err(|e| format!("delete network instance: {e}"))?;
    println!("deleted {inst_id}");
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take_opt = |name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        let value = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        value
    };
    let rpc = take_opt("--rpc").unwrap_or_else(|| "127.0.0.1:15888".to_string());
    let wait = take_opt("--wait").map(|s| s.parse::<u64>());

    let result = async {
        let wait = Duration::from_secs(
            wait.transpose()
                .map_err(|e| format!("bad --wait: {e}"))?
                .unwrap_or(20),
        );
        let url = format!("tcp://{rpc}")
            .parse()
            .map_err(|e| format!("bad --rpc {rpc}: {e}"))?;
        let mut client = runtime_rpc_client(url);
        match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["run", path] => run(&mut client, path, wait).await,
            ["delete", id] => delete(&mut client, id).await,
            _ => Err(USAGE.to_string()),
        }
    }
    .await;

    result.unwrap_or_else(|e| {
        eprintln!("fork_instance_ctl: {e}");
        ExitCode::from(2)
    })
}

use dotenv::dotenv;
use futures::stream::select;
use futures::{pin_mut, TryStreamExt};
use hcloud::apis::configuration::Configuration;
use hcloud::models::{AssignFloatingIpToServerRequest, FloatingIp};
use k8s_openapi::api::core::v1::{Node as KubeNode, Service as KubeService};
use kube::api::ListParams;
use kube::runtime::{watcher, WatchStreamExt};
use kube::{Api, Client as KubeClient};
use rand::seq::SliceRandom;
use std::collections::HashSet;
use std::env;
use std::error::Error as StdError;
use std::fmt::Debug;

type Error = Box<dyn StdError>;

#[derive(Debug)]
enum KubeResource {
    Node(KubeNode),
    Service(KubeService),
}

fn is_load_balancer(service: &KubeService) -> bool {
    service.spec.as_ref().unwrap().type_.as_ref().unwrap() == "LoadBalancer"
}

fn get_hc_server_id(node: &KubeNode) -> i32 {
    let provider_id = node.spec.as_ref().unwrap().provider_id.as_ref().unwrap();
    provider_id
        .strip_prefix("hcloud://")
        .map(|a| a.parse::<i32>().unwrap())
        .unwrap()
}

async fn fetch_available_hc_server_ids(nodes_api: &Api<KubeNode>) -> Result<HashSet<i32>, Error> {
    let nodes = nodes_api.list(&ListParams::default()).await?;
    Ok(nodes
        .iter()
        .filter(|node| {
            node.spec
                .as_ref()
                .unwrap()
                .unschedulable
                .map(|unschedulable| !unschedulable)
                .unwrap_or(true)
        })
        .map(get_hc_server_id)
        .collect())
}

async fn assign_floating_ip_to_server(
    hcloud_conf: &Configuration,
    fip_id: &i32,
    server_id: &i32,
) -> Result<(), Error> {
    println!("assigning {} to {}", fip_id, server_id);
    hcloud::apis::floating_ips_api::assign_floating_ip_to_server(
        hcloud_conf,
        hcloud::apis::floating_ips_api::AssignFloatingIpToServerParams {
            id: *fip_id,
            assign_floating_ip_to_server_request: Some(AssignFloatingIpToServerRequest {
                server: *server_id,
            }),
        },
    )
    .await?;
    Ok(())
}

async fn fetch_floating_ips(hcloud_conf: &Configuration) -> Result<Vec<FloatingIp>, Error> {
    let fips = hcloud::apis::floating_ips_api::list_floating_ips(
        hcloud_conf,
        hcloud::apis::floating_ips_api::ListFloatingIpsParams::default(),
    )
    .await?
    .floating_ips;
    Ok(fips)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    let hcloud_token = env::var("HCLOUD_TOKEN")
        .map_err(|_| "Missing environment variable HCLOUD_TOKEN")
        .unwrap();

    let mut hcloud_conf = Configuration::new();
    hcloud_conf.bearer_access_token = Some(hcloud_token);

    let kube_client = KubeClient::try_default().await.unwrap();
    let services_api = Api::<KubeService>::all(kube_client.clone());
    let nodes_api = Api::<KubeNode>::all(kube_client.clone());

    let nodes_stream = watcher(nodes_api.clone(), ListParams::default()).applied_objects();
    let services_stream = watcher(services_api.clone(), ListParams::default()).applied_objects();
    let stream = select(
        nodes_stream.map_ok(KubeResource::Node),
        services_stream.map_ok(KubeResource::Service),
    );
    pin_mut!(stream);

    while let Some(resource) = stream.try_next().await? {
        match resource {
            KubeResource::Node(node) => {
                let spec = node.spec.as_ref().unwrap();
                if !spec.unschedulable.unwrap_or(false) {
                    continue;
                }

                println!(
                    "node {} is unschedulable, finding it's assigned floating ips",
                    node.metadata.name.as_ref().unwrap()
                );

                let server_id = get_hc_server_id(&node);

                let floating_ips_to_reassign: Vec<_> = fetch_floating_ips(&hcloud_conf)
                    .await?
                    .into_iter()
                    .filter(|fip| fip.server.map(|id| id == server_id).unwrap_or(false))
                    .collect();

                let available_hc_server_ids = fetch_available_hc_server_ids(&nodes_api).await?;

                for fip in floating_ips_to_reassign {
                    let ids = available_hc_server_ids.iter().collect::<Vec<_>>();
                    let server_id = ids.choose(&mut rand::thread_rng()).unwrap();
                    assign_floating_ip_to_server(&hcloud_conf, &fip.id, server_id).await?;
                }
            }
            KubeResource::Service(service) => {
                if !is_load_balancer(&service) {
                    continue;
                }

                let ips: HashSet<_> = service
                    .status
                    .as_ref()
                    .and_then(|s| s.load_balancer.as_ref())
                    .and_then(|lb| lb.ingress.as_ref())
                    .map(|ingress| ingress.iter().flat_map(|i| i.ip.as_ref()).collect())
                    .unwrap_or_default();

                let floating_ips = hcloud::apis::floating_ips_api::list_floating_ips(
                    &hcloud_conf,
                    hcloud::apis::floating_ips_api::ListFloatingIpsParams::default(),
                )
                .await?
                .floating_ips
                .into_iter()
                .filter(|fip| ips.contains(&fip.ip));

                let available_hc_server_ids = fetch_available_hc_server_ids(&nodes_api).await?;

                let floating_ips_to_rassign: Vec<_> = floating_ips
                    .filter(|fip| {
                        fip.server
                            .map(|server| !available_hc_server_ids.contains(&server))
                            .unwrap_or(true)
                    })
                    .collect();

                for fip in floating_ips_to_rassign {
                    let server_id = *available_hc_server_ids.iter().next().unwrap();
                    println!("Reassigning {} to {}", fip.ip, server_id);
                    assign_floating_ip_to_server(&hcloud_conf, &fip.id, &server_id).await?;
                }
            }
        }
    }

    Ok(())
}

// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use futures::stream::BoxStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;

use mz_orchestrator::{
    LabelSelectionLogic, LabelSelector, NamespacedOrchestrator, Service, ServiceConfig,
    ServiceEvent, ServicePort, ServiceProcessMetrics,
};

use super::{
    ComputeInstanceEvent, ComputeInstanceId, ComputeReplicaAllocation, ComputeReplicaLocation,
    ReplicaId,
};

#[derive(Clone, Debug)]
pub(super) struct ComputeOrchestrator {
    inner: Arc<dyn NamespacedOrchestrator>,
    clusterd_image: String,
    init_container_image: Option<String>,
}

impl ComputeOrchestrator {
    pub(super) fn new(
        inner: Arc<dyn NamespacedOrchestrator>,
        clusterd_image: String,
        init_container_image: Option<String>,
    ) -> Self {
        Self {
            inner,
            clusterd_image,
            init_container_image,
        }
    }

    /// Ensure that a service for the given replica exists at the given `location`.
    ///
    /// Returns a tuple `(compute_addrs, workers, timely_addrs)`, where:
    ///  * `command_addrs` is the list of addresses on which the replica's processes listen for
    ///    controller connections.
    ///  * `workers` is the number of timely workers per process.
    ///  * `timely_addrs` is the list of addresses used by the timely cluster for inter-process
    ///    communication.
    pub(super) async fn ensure_replica_location(
        &self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
        location: ComputeReplicaLocation,
    ) -> Result<(Vec<String>, usize, Vec<String>), anyhow::Error> {
        match location {
            ComputeReplicaLocation::Remote {
                addrs,
                compute_addrs,
                workers,
            } => {
                let server_addrs = addrs.into_iter().collect();
                let workers = workers.get();
                let worker_addrs = compute_addrs.into_iter().collect();
                Ok((server_addrs, workers, worker_addrs))
            }
            ComputeReplicaLocation::Managed {
                allocation,
                availability_zone,
                ..
            } => {
                let service = self
                    .ensure_replica(instance_id, replica_id, allocation, availability_zone)
                    .await?;

                let command_addrs = service.addresses("computectl");
                let workers = allocation.workers.get();
                let timely_addrs = service.addresses("compute");
                Ok((command_addrs, workers, timely_addrs))
            }
        }
    }

    pub(super) async fn ensure_replica(
        &self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
        allocation: ComputeReplicaAllocation,
        availability_zone: String,
    ) -> Result<Box<dyn Service>, anyhow::Error> {
        let service_name = generate_replica_service_name(instance_id, replica_id);

        let service = self
            .inner
            .ensure_service(
                &service_name,
                ServiceConfig {
                    image: self.clusterd_image.clone(),
                    init_container_image: self.init_container_image.clone(),
                    args: &|assigned| {
                        vec![
                            format!(
                                "--storage-controller-listen-addr={}",
                                assigned["storagectl"]
                            ),
                            format!(
                                "--compute-controller-listen-addr={}",
                                assigned["computectl"]
                            ),
                            format!("--internal-http-listen-addr={}", assigned["internal-http"]),
                            format!("--opentelemetry-resource=instance_id={}", instance_id),
                            format!("--opentelemetry-resource=replica_id={}", replica_id),
                        ]
                    },
                    ports: vec![
                        ServicePort {
                            name: "storagectl".into(),
                            port_hint: 2100,
                        },
                        ServicePort {
                            name: "computectl".into(),
                            port_hint: 2101,
                        },
                        ServicePort {
                            name: "compute".into(),
                            port_hint: 2102,
                        },
                        ServicePort {
                            name: "internal-http".into(),
                            port_hint: 6878,
                        },
                    ],
                    cpu_limit: allocation.cpu_limit,
                    memory_limit: allocation.memory_limit,
                    scale: allocation.scale,
                    labels: HashMap::from([
                        ("replica-id".into(), replica_id.to_string()),
                        ("cluster-id".into(), instance_id.to_string()),
                        ("type".into(), "cluster".into()),
                    ]),
                    availability_zone: Some(availability_zone),
                    // This constrains the orchestrator (for those orchestrators that support
                    // anti-affinity, today just k8s) to never schedule pods for different replicas
                    // of the same cluster on the same node. Pods from the _same_ replica are fine;
                    // pods from different clusters are also fine.
                    //
                    // The point is that if pods of two replicas are on the same node, that node
                    // going down would kill both replicas, and so the replication factor of the
                    // cluster in question is illusory.
                    anti_affinity: Some(vec![
                        LabelSelector {
                            label_name: "cluster-id".to_string(),
                            logic: LabelSelectionLogic::Eq {
                                value: instance_id.to_string(),
                            },
                        },
                        LabelSelector {
                            label_name: "replica-id".into(),
                            logic: LabelSelectionLogic::NotEq {
                                value: replica_id.to_string(),
                            },
                        },
                    ]),
                },
            )
            .await?;

        Ok(service)
    }

    pub(super) async fn drop_replica(
        &self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
    ) -> Result<(), anyhow::Error> {
        let service_name = generate_replica_service_name(instance_id, replica_id);
        self.inner.drop_service(&service_name).await
    }

    pub(super) async fn list_replicas(
        &self,
    ) -> Result<impl Iterator<Item = (ComputeInstanceId, ReplicaId)>, anyhow::Error> {
        self.inner.list_services().await.map(|s| {
            s.into_iter()
                .filter_map(|x| parse_replica_service_name(&x).ok())
        })
    }

    pub(super) fn watch_services(&self) -> BoxStream<'static, ComputeInstanceEvent> {
        fn translate_event(event: ServiceEvent) -> Result<ComputeInstanceEvent, anyhow::Error> {
            let (instance_id, replica_id) = parse_replica_service_name(&event.service_id)?;
            Ok(ComputeInstanceEvent {
                instance_id,
                replica_id,
                process_id: event.process_id,
                status: event.status,
                time: event.time,
            })
        }

        let stream = self
            .inner
            .watch_services()
            .map(|event| event.and_then(translate_event))
            .filter_map(|event| async {
                match event {
                    Ok(event) => Some(event),
                    Err(error) => {
                        tracing::error!("service watch error: {error}");
                        None
                    }
                }
            });

        Box::pin(stream)
    }

    pub(super) async fn fetch_replica_metrics(
        &self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
    ) -> Result<Vec<ServiceProcessMetrics>, anyhow::Error> {
        let name = generate_replica_service_name(instance_id, replica_id);
        self.inner.fetch_service_metrics(&name).await
    }
}

/// Deterministically generates replica names based on inputs.
fn generate_replica_service_name(instance_id: ComputeInstanceId, replica_id: ReplicaId) -> String {
    format!("cluster-{instance_id}-replica-{replica_id}")
}

/// Parse a name generated by `generate_replica_service_name`, to extract the
/// replica's compute instance ID and replica ID values.
fn parse_replica_service_name(
    service_name: &str,
) -> Result<(ComputeInstanceId, ReplicaId), anyhow::Error> {
    static SERVICE_NAME_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?-u)^cluster-([us]\d+)-replica-(\d+)$").unwrap());

    let caps = SERVICE_NAME_RE
        .captures(service_name)
        .ok_or_else(|| anyhow!("invalid service name: {service_name}"))?;

    let instance_id = caps.get(1).unwrap().as_str().parse().unwrap();
    let replica_id = caps.get(2).unwrap().as_str().parse().unwrap();
    Ok((instance_id, replica_id))
}

//! Phase 0 prototype: confirms the OPC UA server stack works end-to-end.
//!
//! Starts an OPC UA server (opc.tcp://0.0.0.0:4841) exposing a couple of variables under a
//! "Gateway" folder, one of which ticks every 500ms - standing in for a future Modbus-sourced
//! tag. Connect any OPC UA client (e.g. UaExpert) to see it live. Run with:
//!   cargo run -p opcua-gateway --example hello_server
//!
//! Port 4841 (not the standard 4840) is used here only because 4840 is occupied by an
//! unrelated process on the dev machine this was written on - the real product's default
//! stays 4840 (see server.conf / PLAN.md §5).

use std::sync::{
    atomic::{AtomicI32, Ordering},
    Arc,
};
use std::time::Duration;

use log::warn;
use opcua::server::address_space::Variable;
use opcua::server::diagnostics::NamespaceMetadata;
use opcua::server::node_manager::memory::{
    simple_node_manager, InMemoryNodeManager, SimpleNodeManager, SimpleNodeManagerImpl,
};
use opcua::server::{ServerBuilder, SubscriptionCache};
use opcua::types::{BuildInfo, DataValue, DateTime, NodeId, UAString};

const CONFIG_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/server.conf");

#[tokio::main]
async fn main() {
    env_logger::init();

    let (server, handle) = ServerBuilder::new()
        .with_config_from(CONFIG_PATH)
        .build_info(BuildInfo {
            product_uri: "urn:opc-modbus-gateway:prototype".into(),
            manufacturer_name: "OPC Modbus Gateway".into(),
            product_name: "OPC Modbus Gateway (phase 0 prototype)".into(),
            software_version: "0.1.0".into(),
            build_number: "0".into(),
            build_date: DateTime::now(),
        })
        .with_node_manager(simple_node_manager(
            NamespaceMetadata {
                namespace_uri: "urn:OpcModbusGateway".to_owned(),
                ..Default::default()
            },
            "gateway",
        ))
        .trust_client_certs(true)
        .diagnostics_enabled(true)
        .build()
        .unwrap();

    let node_manager = handle
        .node_managers()
        .get_of_type::<SimpleNodeManager>()
        .unwrap();
    let ns = handle.get_namespace_index("urn:OpcModbusGateway").unwrap();
    println!("urn:OpcModbusGateway namespace index: {ns}");

    add_gateway_variables(ns, node_manager, handle.subscriptions().clone());

    let handle_c = handle.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!("Failed to register CTRL-C handler: {e}");
            return;
        }
        handle_c.cancel();
    });

    println!("OPC UA server listening on opc.tcp://0.0.0.0:4841/ - Ctrl+C to stop");
    server.run().await.unwrap();
}

/// Stand-ins for tags that will later be sourced from the Modbus poller (see PLAN.md §5).
fn add_gateway_variables(
    ns: u16,
    manager: Arc<InMemoryNodeManager<SimpleNodeManagerImpl>>,
    subscriptions: Arc<SubscriptionCache>,
) {
    let info_node = NodeId::new(ns, "Gateway.Info");
    let counter_node = NodeId::new(ns, "Gateway.Counter");

    let address_space = manager.address_space();
    {
        let mut address_space = address_space.write();

        let folder_id = NodeId::new(ns, "folder");
        address_space.add_folder(&folder_id, "Gateway", "Gateway", &NodeId::objects_folder_id());

        let _ = address_space.add_variables(
            vec![
                Variable::new(
                    &info_node,
                    "Gateway.Info",
                    "Gateway.Info",
                    UAString::from("OPC Modbus Gateway - phase 0 prototype"),
                ),
                Variable::new(&counter_node, "Gateway.Counter", "Gateway.Counter", 0_i32),
            ],
            &folder_id,
        );
    }

    tokio::task::spawn(async move {
        let counter = AtomicI32::new(0);
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            manager
                .set_values(
                    &subscriptions,
                    [(
                        &counter_node,
                        None,
                        DataValue::new_now(counter.fetch_add(1, Ordering::Relaxed)),
                    )]
                    .into_iter(),
                )
                .unwrap();
        }
    });
}

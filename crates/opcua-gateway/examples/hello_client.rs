//! Phase 0 prototype: proves an OPC UA client can actually connect, subscribe and receive
//! live data-change notifications from `hello_server` - not just that the TCP port is open.
//! Start the server first, then run:
//!   cargo run -p opcua-gateway --example hello_client

use std::time::Duration;

use opcua::client::{ClientBuilder, DataChangeCallback, MonitoredItem, IdentityToken};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    DataValue, MessageSecurityMode, MonitoredItemCreateRequest, NodeId, TimestampsToReturn,
    UserTokenPolicy,
};

/// Namespace index the server printed at startup for "urn:OpcModbusGateway".
const GATEWAY_NS: u16 = 2;

#[tokio::main]
async fn main() {
    eprintln!("Building client...");
    let mut client = ClientBuilder::new()
        .application_name("Gateway phase-0 test client")
        .application_uri("urn:OpcModbusGatewayTestClient")
        .trust_server_certs(true)
        .create_sample_keypair(true)
        .session_retry_limit(1)
        .client()
        .unwrap();
    eprintln!("Client built. Connecting...");

    let (session, event_loop) = tokio::time::timeout(
        Duration::from_secs(10),
        // Use the literal IP, not "localhost" - on this dev machine "localhost" resolution
        // made the connect hang indefinitely with zero CPU usage and no socket ever opened.
        client.connect_to_matching_endpoint(
            (
                "opc.tcp://127.0.0.1:4841",
                SecurityPolicy::None.to_str(),
                MessageSecurityMode::None,
                UserTokenPolicy::anonymous(),
            ),
            IdentityToken::Anonymous,
        ),
    )
    .await
    .expect("connect_to_matching_endpoint timed out after 10s")
    .unwrap();
    eprintln!("Endpoint matched, session created. Spawning event loop...");
    let handle = event_loop.spawn();
    tokio::time::timeout(Duration::from_secs(10), session.wait_for_connection())
        .await
        .expect("wait_for_connection timed out after 10s");
    eprintln!("Connected.");

    let subscription_id = session
        .create_subscription(
            Duration::from_millis(500),
            10,
            30,
            0,
            0,
            true,
            DataChangeCallback::new(|dv, item| print_value(&dv, item)),
        )
        .await
        .unwrap();
    println!("Created subscription {subscription_id}");

    let items: Vec<MonitoredItemCreateRequest> = ["Gateway.Info", "Gateway.Counter"]
        .iter()
        .map(|id| NodeId::new(GATEWAY_NS, *id).into())
        .collect();
    session
        .create_monitored_items(subscription_id, TimestampsToReturn::Both, items)
        .await
        .unwrap();

    // Collect a handful of updates then exit - this is a one-shot sanity check, not a daemon.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = session.disconnect().await;
    handle.abort();
    println!("Done.");
}

fn print_value(data_value: &DataValue, item: &MonitoredItem) {
    let node_id = &item.item_to_monitor().node_id;
    match &data_value.value {
        Some(value) => println!("Data change: {node_id} = {value:?}"),
        None => println!("Data change: {node_id} has no value (status {:?})", data_value.status),
    }
}

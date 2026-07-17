//! B2 e2e: the server BINDS on `opcua.host` while GetEndpoints returns
//! endpoint URLs and discovery URLs built from `opcua.advertised_host`.
//!
//! The proof is the combination: the client reaches the server via
//! 127.0.0.1 (so the socket is bound there), yet every URL it receives
//! names the advertised host (so the bind host never leaks to clients).

use std::sync::Arc;
use std::time::Duration;

use opcua::client::ClientBuilder;
use tokio::net::TcpListener;

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_endpoints_advertises_advertised_host_while_binding_on_host() {
    let port = free_port().await;
    let pki = tempfile::tempdir().unwrap();

    // Bind on loopback; advertise a name only clients-side DNS would know.
    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "opcua": {{
            "host": "127.0.0.1",
            "advertised_host": "scada-gw.example",
            "port": {port},
            "pki_dir": {pki_dir:?}
        }},
        "poll_groups": [],
        "channels": []
    }}"#,
        pki_dir = pki.path().join("pki"),
    ))
    .expect("config loads");

    let (poller, cache) = mb_poller::Poller::spawn_with_cache(&cfg);
    let engine = tags_core::TagEngine::new(
        &cfg,
        Arc::clone(&cache) as Arc<dyn mb_poller::CacheReader>,
        None,
    )
    .unwrap();
    let (engine_handle, typed) = engine.spawn();
    let opcua = opcua_gateway::spawn(
        &cfg,
        typed as Arc<dyn tags_core::TypedReader>,
        poller.all_writers(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The handle's own endpoint URL is the advertised one.
    assert_eq!(
        opcua.endpoint_url(),
        format!("opc.tcp://scada-gw.example:{port}")
    );

    // A real GetEndpoints against the BIND address (this connecting at all
    // proves the socket is on 127.0.0.1, not on the advertised name).
    let client = ClientBuilder::new()
        .application_name("advertise test client")
        .application_uri("urn:opc-modbus-server:advertise-client")
        .trust_server_certs(true)
        .create_sample_keypair(false)
        .pki_dir(pki.path().join("client-pki").to_string_lossy().to_string())
        .session_retry_limit(1)
        .client()
        .unwrap();
    let endpoints = tokio::time::timeout(
        Duration::from_secs(15),
        client.get_server_endpoints_from_url(format!("opc.tcp://127.0.0.1:{port}")),
    )
    .await
    .expect("GetEndpoints timed out")
    .expect("GetEndpoints failed");

    assert!(!endpoints.is_empty(), "server offers endpoints");
    let advertised_base = format!("opc.tcp://scada-gw.example:{port}");
    for ep in &endpoints {
        let url = ep.endpoint_url.as_ref();
        assert!(
            url.starts_with(&advertised_base),
            "endpoint URL must advertise advertised_host, got {url}"
        );
        let discovery: Vec<String> = ep
            .server
            .discovery_urls
            .iter()
            .flatten()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            discovery,
            vec![format!("{advertised_base}/")],
            "discovery URLs must advertise advertised_host"
        );
    }

    // Teardown.
    opcua.shutdown().await;
    engine_handle.shutdown().await;
    tokio::time::timeout(Duration::from_secs(5), poller.shutdown())
        .await
        .expect("poller shutdown");
}

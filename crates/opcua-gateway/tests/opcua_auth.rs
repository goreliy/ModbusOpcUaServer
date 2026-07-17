//! Phase-5 authentication e2e: username/password sessions against the real
//! server (no Modbus needed — zero channels), argon2-hashed credentials.

use std::sync::Arc;
use std::time::Duration;

use opcua::client::{ClientBuilder, IdentityToken};
use opcua::crypto::SecurityPolicy;
use opcua::types::{MessageSecurityMode, UAString, UserTokenPolicy, UserTokenType};

fn username_policy() -> UserTokenPolicy {
    UserTokenPolicy {
        policy_id: UAString::null(),
        token_type: UserTokenType::UserName,
        issued_token_type: UAString::null(),
        issuer_endpoint_url: UAString::null(),
        security_policy_uri: UAString::null(),
    }
}
use tokio::net::TcpListener;

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

fn client(pki: &std::path::Path) -> opcua::client::Client {
    ClientBuilder::new()
        .application_name("auth test client")
        .application_uri("urn:opc-modbus-server:auth-client")
        .trust_server_certs(true)
        .create_sample_keypair(false)
        .pki_dir(pki.join("client-pki").to_string_lossy().to_string())
        .session_retry_limit(1)
        .client()
        .unwrap()
}

async fn refused<T: opcua::client::transport::Connector + Send + Sync + 'static>(
    res: Result<
        (Arc<opcua::client::Session>, opcua::client::SessionEventLoop<T>),
        opcua::types::Error,
    >,
) -> bool {
    match res {
        Err(_) => true,
        Ok((session, event_loop)) => {
            let handle = event_loop.spawn();
            // wait_for_connection resolves `false` once the retry budget is
            // exhausted; a hang beyond 10s counts as refused too. NEVER call
            // disconnect() on a session that did not connect — it hangs.
            let connected = tokio::time::timeout(
                Duration::from_secs(10),
                session.wait_for_connection(),
            )
            .await
            .unwrap_or(false);
            handle.abort();
            !connected
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn username_password_sessions_with_argon2_hash() {
    let port = free_port().await;
    let pki = tempfile::tempdir().unwrap();
    let phc = opcua_gateway::hash_password("operator123").expect("hash");

    // Anonymous OFF; one user with a HASHED password.
    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "opcua": {{
            "host": "127.0.0.1",
            "port": {port},
            "allow_anonymous": false,
            "users": [ {{ "username": "operator", "password_hash": {phc:?} }} ],
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

    let endpoint = format!("opc.tcp://127.0.0.1:{port}");

    // Correct credentials: session comes up.
    let (session, event_loop) = tokio::time::timeout(
        Duration::from_secs(15),
        client(pki.path()).connect_to_matching_endpoint(
            (
                endpoint.as_str(),
                SecurityPolicy::None.to_str(),
                MessageSecurityMode::None,
                username_policy(),
            ),
            IdentityToken::UserName("operator".into(), "operator123".into()),
        ),
    )
    .await
    .expect("connect timed out")
    .expect("endpoint connect failed");
    let handle = event_loop.spawn();
    let connected = tokio::time::timeout(Duration::from_secs(10), session.wait_for_connection())
        .await
        .expect("session connect timed out");
    assert!(connected, "correct credentials must connect");
    let _ = session.disconnect().await;
    handle.abort();

    // Wrong password: the session must NOT come up.
    let res = tokio::time::timeout(
        Duration::from_secs(15),
        client(pki.path()).connect_to_matching_endpoint(
            (
                endpoint.as_str(),
                SecurityPolicy::None.to_str(),
                MessageSecurityMode::None,
                username_policy(),
            ),
            IdentityToken::UserName("operator".into(), "wrong".into()),
        ),
    )
    .await
    .expect("connect attempt timed out");
    assert!(refused(res).await, "wrong password must not produce a session");

    // Anonymous: refused too (allow_anonymous = false).
    let res = tokio::time::timeout(
        Duration::from_secs(15),
        client(pki.path()).connect_to_matching_endpoint(
            (
                endpoint.as_str(),
                SecurityPolicy::None.to_str(),
                MessageSecurityMode::None,
                UserTokenPolicy::anonymous(),
            ),
            IdentityToken::Anonymous,
        ),
    )
    .await
    .expect("connect attempt timed out");
    assert!(refused(res).await, "anonymous must be refused when allow_anonymous=false");

    opcua.shutdown().await;
    engine_handle.shutdown().await;
    tokio::time::timeout(Duration::from_secs(5), poller.shutdown())
        .await
        .expect("poller shutdown");
}

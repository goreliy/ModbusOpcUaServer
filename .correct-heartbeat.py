from pathlib import Path
p=Path('crates/svc-host/src/mcp/transport.rs');s=p.read_text(encoding='utf-8').replace('std::time::Duration::from_secs(3),\n        tokio::net::TcpStream::connect','std::time::Duration::from_millis(750),\n        tokio::net::TcpStream::connect',1);start=s.index('    let status = remote(',s.index('async fn bridge_connect'));end=s.index('    if let Some(initialize)',start)
s=s[:start]+'''    let reply = tokio::time::timeout(std::time::Duration::from_secs(1),
        remote(&endpoint,&json!({"jsonrpc":"2.0","id":0,"method":"ping"})))
        .await.map_err(|_|"Selected controller is not responding".to_owned())??.ok_or("Empty controller ping")?;
    if reply.get("result").is_none() { return Err("Selected controller rejected MCP ping".into()); }
'''+s[end:]
s=s.replace('    let result = async {\n        loop {\n            let line = tokio::select!', '    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);\n    let result = async {\n        loop {\n            let line = tokio::select!')
s=s.replace('''            let line = tokio::select! {
                line =''','''            let line = tokio::select! {
                biased;
                line =''')
p.write_text(s,encoding='utf-8')

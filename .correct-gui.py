from pathlib import Path
p=Path('crates/gui/src/server.rs');s=p.read_text(encoding='utf-8');pos=s.index('/// Кольцевой буфер')
s=s[:pos]+'''async fn attach_controller(path: std::path::PathBuf) -> Result<GuiStack, String> {
    match svc_host::mcp::Controller::new(&path).await {
        Ok(controller) => Ok(GuiStack::Managed(controller)),
        Err(error) if error == svc_host::mcp::OWNER_BUSY => {
            crate::remote::Remote::connect(path).await.map(GuiStack::Remote)
        }
        Err(error) => Err(error),
    }
}

'''+s[pos:]
s=s.replace('''                match crate::remote::Remote::connect(path).await {
                    Ok(c) => Ev::Started(Box::new(GuiStack::Remote(c)), Instant::now()),''','''                match attach_controller(path).await {
                    Ok(stack) => Ev::Started(Box::new(stack), Instant::now()),''',1)
old='''                    return match crate::remote::Remote::connect(path).await {
                        Ok(c) => match c.start().await {
                            Ok(()) => Ev::Started(Box::new(GuiStack::Remote(c)), Instant::now()),
                            Err(e) => {
                                c.detach().await;
                                Ev::StartFailed(e)
                            }
                        },
                        Err(e) => Ev::StartFailed(e),
                    };'''
new='''                    return match attach_controller(path).await {
                        Ok(stack) => {
                            let result = match &stack {
                                GuiStack::Managed(c) => c.start_server().await,
                                GuiStack::Remote(c) => c.start().await,
                                _ => unreachable!(),
                            };
                            match result {
                                Ok(()) => Ev::Started(Box::new(stack), Instant::now()),
                                Err(e) => { stack.shutdown().await; Ev::StartFailed(e) }
                            }
                        }
                        Err(e) => Ev::StartFailed(e),
                    };'''
assert old in s;s=s.replace(old,new)
s=s.replace('''    pub fn pump(&mut self) -> bool {
        if let ServerState''','''    pub fn pump(&mut self) -> bool {
        let lost = matches!(&self.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Managed(c) if c.is_closed()));
        let retry = matches!(&self.state,ServerState::Stopped) && self.last_check.elapsed() >= std::time::Duration::from_secs(2);
        if (lost || retry) && self.watch_path.is_some() {
            let path = self.watch_path.clone().unwrap();
            self.last_check = Instant::now();
            self.attach(path);
            return true;
        }
        if let ServerState''')
# Managed controllers own their file watcher; GUI follows edits but must not apply twice.
s=s.replace('if matches!(&**stack,GuiStack::Remote(_)))','if matches!(&**stack,GuiStack::Remote(_)|GuiStack::Managed(_)))')
p.write_text(s,encoding='utf-8')
p=Path('crates/gui/src/remote.rs');s=p.read_text(encoding='utf-8').replace('Client::connect_or_start(&path).await?', 'Client::wait(&path, Duration::from_secs(2)).await?').replace('Client::connect_or_start(&owner.path).await','Client::connect(&owner.path).await').replace('Client::connect_or_start(&self.path).await?', 'Client::wait(&self.path, Duration::from_secs(2)).await?');p.write_text(s,encoding='utf-8')
p=Path('crates/gui/src/app.rs');s=p.read_text(encoding='utf-8').replace('''                ui.label(format!(
                    "Программа: {}",''','''                ui.label(format!("Экземпляр: {} · {}", info["instance_name"].as_str().unwrap_or(""), info["config_id"].as_str().unwrap_or("")));
                ui.label(format!(
                    "Программа: {}",''');s=s.replace('''                    ui.label("Запуск...");''','''                    ui.label("Подключение к выбранному экземпляру...");''');s=s.replace('''                        "Конфиг не сохранён — относительные пути (./data, ./logs, pki) \\
                         будут считаться от текущего каталога.",''','''                        if self.path.is_none() { "Сохраните конфигурацию, чтобы подключить MCP-клиент к этому экземпляру." } else { "Есть несохранённые изменения. Рабочий сервер использует сохранённую конфигурацию." },''');s=s.replace('// Detach this window; the persistent MCP host keeps polling.','// Close this owner, or detach from a separately running server/service.');
p.write_text(s,encoding='utf-8')
p=Path('crates/app/src/main.rs');s=p.read_text(encoding='utf-8').replace('MCP over stdio: attach to this config\'s running controller or own an idle one.', 'MCP over stdio for Codex/Claude: wait for and attach only to the selected config\'s GUI/server.');p.write_text(s,encoding='utf-8')

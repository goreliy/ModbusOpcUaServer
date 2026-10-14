use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};
use svc_host::mcp::{Client, ServerView};
use tags_core::TypedReader;
use tokio::sync::watch;

pub struct Remote {
    pub path: PathBuf,
    client: parking_lot::RwLock<Client>,
    view: parking_lot::RwLock<ServerView>,
    pub logs: parking_lot::RwLock<Vec<String>>,
    stop: watch::Sender<bool>,
    connected: std::sync::atomic::AtomicBool,
    running_intent: std::sync::atomic::AtomicBool,
    task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Remote {
    pub async fn connect(path: PathBuf) -> Result<Arc<Self>, String> {
        let client = Client::wait(&path, Duration::from_secs(2)).await?;
        let view = client.view().await?;
        let (stop, mut exit) = watch::channel(false);
        let remote = Arc::new(Self {
            path,
            client: parking_lot::RwLock::new(client),
            view: parking_lot::RwLock::new(view.clone()),
            logs: parking_lot::RwLock::new(vec![]),
            stop,
            connected: std::sync::atomic::AtomicBool::new(true),
            running_intent: std::sync::atomic::AtomicBool::new(view.running),
            task: parking_lot::Mutex::new(None),
        });
        let owner = remote.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {biased;_ = exit.changed()=>break,_ = tick.tick()=>{}}
                let client = owner.client.read().clone();
                let result =
                    tokio::select! {biased;_ = exit.changed()=>break,result=client.view()=>result};
                match result {
                    Ok(view) => {
                        owner
                            .running_intent
                            .store(view.running, std::sync::atomic::Ordering::Relaxed);
                        owner
                            .connected
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        *owner.view.write() = view;
                    }
                    Err(error) => {
                        owner
                            .connected
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        {
                            let mut view = owner.view.write();
                            view.running = false;
                            view.mcp_clients.clear();
                            view.error = Some(format!("Соединение с сервером: {error}"));
                            if let Some(store) = &view.typed {
                                for (id, mut value) in store.snapshot_all() {
                                    value.quality = mb_poller::Quality::Bad;
                                    store.seed(id, value);
                                }
                            }
                        }
                        if let Ok(client) = Client::connect(&owner.path).await {
                            *owner.client.write() = client;
                        }
                    }
                }
                let client = owner.client.read().clone();
                if let Ok(value) = client.call("get_logs", json!({"limit":200})).await {
                    *owner.logs.write() = value["lines"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                }
            }
        });
        *remote.task.lock() = Some(task);
        Ok(remote)
    }
    pub fn running_before_disconnect(&self) -> bool {
        self.running_intent
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn view(&self) -> ServerView {
        self.view.read().clone()
    }
    pub fn url(&self) -> String {
        self.client.read().url()
    }
    pub async fn start(&self) -> Result<(), String> {
        let client = Client::wait(&self.path, Duration::from_secs(2)).await?;
        client.call("server_start", json!({})).await?;
        self.running_intent
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *self.view.write() = client.view().await?;
        *self.client.write() = client;
        Ok(())
    }
    pub async fn stop(&self) -> Result<(), String> {
        let client = self.client.read().clone();
        client.call("server_stop", json!({"confirm":true})).await?;
        self.running_intent
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *self.view.write() = client.view().await?;
        Ok(())
    }
    pub async fn reload(&self) -> Result<(), String> {
        let client = self.client.read().clone();
        let cfg = client.call("get_config", json!({})).await?;
        client
            .call("apply_config", json!({"expected_revision":cfg["revision"]}))
            .await?;
        *self.view.write() = client.view().await?;
        Ok(())
    }
    pub async fn detach(&self) {
        let _ = self.stop.send(true);
        let task = self.task.lock().take();
        if let Some(mut task) = task {
            if tokio::time::timeout(Duration::from_secs(2), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

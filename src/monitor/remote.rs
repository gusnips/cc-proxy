use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use tokio::{sync::watch, task::JoinHandle, time::MissedTickBehavior};

use super::snapshot::{MonitorResponse, MonitorSnapshot, PROTOCOL_VERSION, SnapshotUpdate};

const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Owns polling; dropping the dashboard cancels only its outstanding read.
pub struct RemoteMonitor {
    updates: watch::Receiver<SnapshotUpdate>,
    poller: JoinHandle<()>,
}

impl RemoteMonitor {
    /// Attaches to a proxy that answers now. A typo in `--url` fails here.
    pub async fn connect(client: Client, base_url: Url) -> Result<Self> {
        let endpoint = monitor_endpoint(&base_url)?;
        let initial = fetch_snapshot(&client, &endpoint)
            .await
            .context("cannot attach to proxy monitor")?;
        Ok(Self::poll(client, endpoint, SnapshotUpdate::Live(initial)))
    }

    /// Starts even when nothing answers yet, and attaches once the proxy is up.
    pub async fn wait_for(client: Client, base_url: Url) -> Result<Self> {
        let endpoint = monitor_endpoint(&base_url)?;
        let initial = match fetch_snapshot(&client, &endpoint).await {
            Ok(snapshot) => SnapshotUpdate::Live(snapshot),
            Err(error) => SnapshotUpdate::Disconnected {
                snapshot: MonitorSnapshot::empty(),
                error: error.to_string(),
            },
        };
        Ok(Self::poll(client, endpoint, initial))
    }

    fn poll(client: Client, endpoint: Url, initial: SnapshotUpdate) -> Self {
        let (sender, updates) = watch::channel(initial.clone());
        let poller = tokio::spawn(async move {
            let mut current = initial;
            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                let result = fetch_snapshot(&client, &endpoint)
                    .await
                    .map_err(|error| error.to_string());
                current = current.updated(result);
                if sender.send(current.clone()).is_err() {
                    break;
                }
            }
        });
        Self { updates, poller }
    }

    pub fn snapshot(&self) -> SnapshotUpdate {
        self.updates.borrow().clone()
    }
}

impl Drop for RemoteMonitor {
    fn drop(&mut self) {
        self.poller.abort();
    }
}

fn monitor_endpoint(base_url: &Url) -> Result<Url> {
    if !matches!(base_url.scheme(), "http" | "https")
        || !base_url.username().is_empty()
        || base_url.password().is_some()
        || base_url.query().is_some()
        || base_url.fragment().is_some()
    {
        bail!("monitor URL must be an HTTP(S) base URL without credentials, query or fragment");
    }
    base_url.join("monitor").context("invalid monitor URL")
}

async fn fetch_snapshot(client: &Client, endpoint: &Url) -> Result<MonitorSnapshot> {
    let mut response = client
        .get(endpoint.clone())
        .send()
        .await?
        .error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_SNAPSHOT_BYTES {
            bail!("monitor snapshot exceeds the size limit");
        }
        body.extend_from_slice(&chunk);
    }
    let response: MonitorResponse =
        serde_json::from_slice(&body).context("invalid monitor snapshot")?;
    if response.version != PROTOCOL_VERSION {
        bail!(
            "unsupported monitor protocol version {}; expected {}",
            response.version,
            PROTOCOL_VERSION
        );
    }
    Ok(response.snapshot)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{Json, Router, routing::get};

    use super::*;

    #[tokio::test]
    async fn waiting_for_an_off_proxy_starts_disconnected_and_attaches_when_it_answers() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = free.local_addr().unwrap();
        drop(free);
        let url: Url = format!("http://{address}").parse().unwrap();
        let client = Client::builder().no_proxy().build().unwrap();

        assert!(
            RemoteMonitor::connect(client.clone(), url.clone())
                .await
                .is_err()
        );
        let monitor = RemoteMonitor::wait_for(client, url).await.unwrap();
        let first = monitor.snapshot();
        assert!(first.connection_error().is_some());
        assert!(first.snapshot().sessions.is_empty());

        let app = Router::new().route(
            "/monitor",
            get(|| async {
                Json(MonitorResponse {
                    version: PROTOCOL_VERSION,
                    snapshot: MonitorSnapshot::empty(),
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut attached = false;
        for _ in 0..40 {
            if monitor.snapshot().connection_error().is_none() {
                attached = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        server.abort();
        assert!(
            attached,
            "the monitor never attached once the proxy answered"
        );
    }
}

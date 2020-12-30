use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Result;
use hyper::client::connect::dns::Name;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::TokioAsyncResolver;

use crate::errors::DnsResolutionFailed;
use crate::task::select_vec;

async fn resolve_host(host: &str) -> Result<IpAddr> {
    let resolver = TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default())?;
    // TODO: ipv6
    let response = resolver.ipv4_lookup(host).await?;
    if let Some(addr) = response.into_iter().next() {
        return Ok(IpAddr::V4(addr));
    }
    Err(anyhow::Error::from(DnsResolutionFailed))
}

#[derive(Clone)]
pub struct DnsResolverServerInner {
    last_resolution_time: Instant,
    last_answer: IpAddr,
}

// garbage collection of hosts is ensured at reloading (reloading generates a new
// DnsResolverServer), and restart resolving from scratch
pub struct DnsResolverServerState {
    hosts: Mutex<HashMap<String, DnsResolverServerInner>>,
    refresh_frequency: Duration,
}

pub type DnsQuerySender = mpsc::Sender<(Name, oneshot::Sender<Option<IpAddr>>)>;
pub type DnsQueryReceiver = mpsc::Receiver<(Name, oneshot::Sender<Option<IpAddr>>)>;

#[derive(Clone)]
pub struct ServiceWrapper<T>(T);
pub type DnsService = ServiceWrapper<DnsQuerySender>;

pub struct ExternalDnsResolverPoller {
    pub poller: Pin<Box<dyn Future<Output = ()>>>,
    pub tx: DnsService,
}

impl ExternalDnsResolverPoller {
    pub fn new(refresh_frequency: Duration) -> Self {
        let state = DnsResolverServerState {
            hosts: Mutex::new(HashMap::new()),
            refresh_frequency,
        };

        let (tx, rx) = mpsc::channel(255);

        let poller = Box::pin(perform_dns_resolution(Arc::new(state), rx));

        ExternalDnsResolverPoller {
            poller,
            tx: ServiceWrapper(tx),
        }
    }
}

async fn name_resolver(
    state: Arc<DnsResolverServerState>,
    requested_host: Name,
    response_tx: oneshot::Sender<Option<IpAddr>>,
) -> anyhow::Result<()> {
    // we cache the dns result so as to not spam our DNS resolver
    let hostname = requested_host.as_str().to_string();
    {
        let hosts = state.hosts.lock().await;
        if let Some(inner) = hosts.get(&hostname) {
            if inner.last_resolution_time.elapsed() < state.refresh_frequency {
                return response_tx
                    .send(Some(inner.last_answer))
                    .map_err(|_e| anyhow::Error::from(DnsResolutionFailed));
            }
        }
    }
    let _ = response_tx.send(match resolve_host(&hostname).await {
        Ok(last_answer) => {
            let mut hosts = state.hosts.lock().await;
            hosts.insert(
                hostname,
                DnsResolverServerInner {
                    last_answer,
                    last_resolution_time: Instant::now(),
                },
            );
            Some(last_answer)
        }
        Err(_) => None,
    });
    Ok(())
}

// TODO: perform graceful shutdown here too
async fn perform_dns_resolution(
    state: Arc<DnsResolverServerState>,
    mut rx: DnsQueryReceiver,
) -> () {
    let mut ongoing_resolutions: Vec<Pin<Box<dyn Future<Output = anyhow::Result<()>>>>> =
        Vec::new();
    loop {
        tokio::select! {
            _ = select_vec(&mut ongoing_resolutions) => {},
            Some((requested_host, response_tx)) = rx.recv() => {
                ongoing_resolutions.push(Box::pin(name_resolver(state.clone(), requested_host, response_tx)));
            }
        }
    }
}

impl tower_service::Service<Name> for ServiceWrapper<DnsQuerySender> {
    type Response = std::iter::Once<SocketAddr>;
    type Error = anyhow::Error;
    type Future = Pin<Box<dyn Future<Output = anyhow::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let clone = self.0.clone();
        let (res_tx, res_rx) = oneshot::channel();
        Box::pin(async move {
            clone.send((name, res_tx)).await?;
            res_rx
                .await
                // maybe not a good idea to loose the distinction between no record and a failing
                // dns resolution
                .unwrap_or(None)
                // install a fake port (I don't think one can listen on the port 0) because hyper will overwrite it later anyway.
                // If that behavior was to change (quite unlikely), we should realize the issue
                // earlier too ;)
                .map(|x| std::iter::once(SocketAddr::from((x, 0))))
                .ok_or(anyhow::Error::from(DnsResolutionFailed))
        })
    }
}

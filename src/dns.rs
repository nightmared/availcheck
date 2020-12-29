use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Result;
use hyper::client::connect::dns::Name;
use tokio::sync::{mpsc, oneshot};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::TokioAsyncResolver;

use crate::errors::DnsResolutionFailed;

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
    hosts: HashMap<String, DnsResolverServerInner>,
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
            hosts: HashMap::new(),
            refresh_frequency,
        };

        let (tx, rx) = mpsc::channel(255);

        let poller = Box::pin(perform_dns_resolution(state, rx));

        ExternalDnsResolverPoller {
            poller,
            tx: ServiceWrapper(tx),
        }
    }
}

// TODO: perform graceful shutdown here too
async fn perform_dns_resolution(mut state: DnsResolverServerState, mut rx: DnsQueryReceiver) -> () {
    for (requested_host, response_tx) in rx.recv().await {
        // we cache the dns result so as to not spam our DNS resolver
        let hostname = requested_host.as_str().to_string();
        if let Some(inner) = state.hosts.get(&hostname) {
            if inner.last_resolution_time.elapsed() < state.refresh_frequency {
                let _ = response_tx.send(Some(inner.last_answer));
                continue;
            }
        }
        let _ = response_tx.send(match resolve_host(&hostname).await {
            Ok(last_answer) => {
                state.hosts.insert(
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

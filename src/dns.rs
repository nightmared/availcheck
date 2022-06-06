use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::client::connect::dns::Name;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::TokioAsyncResolver;

use tracing::instrument;

use crate::{errors::DnsError, task::select_vec};

#[derive(Debug, Clone)]
pub struct DnsResolverServerInner {
    last_resolution_time: Instant,
    last_answer: IpAddr,
}

// garbage collection of hosts is ensured at reloading (reloading generates a new
// DnsResolverServer), and restart resolving from scratch
#[derive(Debug)]
pub struct DnsResolverServerState {
    hosts: HashMap<String, DnsResolverServerInner>,
    join_handles: Vec<tokio::task::JoinHandle<NameResolverAnswer>>,
    refresh_frequency: Duration,
}

pub type DnsQuerySender = mpsc::Sender<(Name, oneshot::Sender<Result<IpAddr, DnsError>>)>;
pub type DnsQueryReceiver = mpsc::Receiver<(Name, oneshot::Sender<Result<IpAddr, DnsError>>)>;

type NameResolverAnswer = (
    Name,
    oneshot::Sender<Result<IpAddr, DnsError>>,
    Result<IpAddr, DnsError>,
);

#[derive(Debug, Clone)]
pub struct ServiceWrapper<T>(T);
pub type DnsService = ServiceWrapper<DnsQuerySender>;

pub struct ExternalDnsResolverPoller(tokio::task::JoinHandle<()>);

impl ExternalDnsResolverPoller {
    pub fn new(refresh_frequency: Duration) -> (Self, DnsService) {
        let (tx, mut rx): (DnsQuerySender, DnsQueryReceiver) = mpsc::channel(255);

        let poller = tokio::spawn(async move {
            let mut state = DnsResolverServerState {
                hosts: HashMap::new(),
                join_handles: Vec::new(),
                refresh_frequency,
            };

            loop {
                tokio::select! {
                    Some(res) = select_vec(&mut state.join_handles) => {
                        match res {
                            Ok((requested_host, tx, query_result)) => {
                                if let Ok(last_answer) = query_result {
                                    state.hosts.insert(
                                        requested_host.to_string(),
                                        DnsResolverServerInner {
                                            last_answer,
                                            last_resolution_time: Instant::now(),
                                        },
                                    );
                                }
                                if let Err(_) = tx.send(query_result) {
                                    tracing::error!(event = "message_sending_failed", host = requested_host.as_str());
                                }
                            },
                            Err(e) => tracing::error!(event = "dns_resolution_failed", error = e.to_string().as_str())
                        }
                    },
                    Some((requested_host, response_tx)) = rx.recv() => {
                        // we cache the dns result so as to not spam our DNS resolver
                        let hostname = requested_host.to_string();

                        if let Some(inner) = state.hosts.get(&hostname) {
                            if inner.last_resolution_time.elapsed() < state.refresh_frequency {
                                if let Err(_) = response_tx.send(Ok(inner.last_answer)) {
                                    tracing::error!(event = "message_sending_failed", host = requested_host.as_str());
                                }
                                continue
                            }
                        }

                        tracing::info!(event = "added_resolver", name = requested_host.as_str());
                        let new_task = tokio::spawn(resolve_host(requested_host, response_tx));
                        state.join_handles.push(new_task);
                    }
                }
            }
        });

        (ExternalDnsResolverPoller(poller), ServiceWrapper(tx))
    }
}

impl Future for ExternalDnsResolverPoller {
    type Output = Result<(), JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl Drop for ExternalDnsResolverPoller {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// cancel all running tasks
impl Drop for DnsResolverServerState {
    fn drop(&mut self) {
        for handle in &mut self.join_handles {
            handle.abort();
        }
    }
}

#[instrument]
async fn resolve_host(
    requested_host: Name,
    response_tx: oneshot::Sender<Result<IpAddr, DnsError>>,
) -> NameResolverAnswer {
    let resolver = TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default());
    let ret = match resolver {
        Ok(resolver) => {
            // TODO: ipv6
            match resolver.ipv4_lookup(requested_host.as_str()).await {
                Ok(response) => {
                    if let Some(addr) = response.into_iter().next() {
                        Ok(IpAddr::V4(addr))
                    } else {
                        Err(DnsError::NoDNSEntry(requested_host.to_string()))
                    }
                }
                Err(e) => Err(DnsError::DnsResolution(e)),
            }
        }
        Err(e) => Err(DnsError::DnsResolution(e)),
    };
    (requested_host, response_tx, ret)
}

impl tower_service::Service<Name> for ServiceWrapper<DnsQuerySender> {
    type Response = std::iter::Once<SocketAddr>;
    type Error = DnsError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, DnsError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), DnsError>> {
        Poll::Ready(Ok(()))
    }

    #[instrument(skip(self))]
    fn call(&mut self, name: Name) -> Self::Future {
        let clone = self.0.clone();
        let (res_tx, res_rx) = oneshot::channel();
        Box::pin(async move {
            clone.send((name, res_tx)).await?;
            res_rx
                .await?
                // install a fake port (I don't think one can listen on the port 0) because hyper will overwrite it later anyway.
                // If that behavior was to change (quite unlikely), we should realize the issue
                // quickly too ;)
                .map(|x| std::iter::once(SocketAddr::from((x, 0))))
        })
    }
}

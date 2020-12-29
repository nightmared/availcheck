use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use c_ares_resolver::FutureResolver;
use hyper::client::connect::dns::Name;
use tokio::sync::Mutex;

use crate::errors::Error;
use crate::task::{TaskClient, TaskClientService, TaskServer, TaskServerOperation};

async fn resolve_host(host: &str) -> Result<IpAddr, Error> {
    match host.parse() {
        Ok(x) => Ok(x),
        Err(_) =>
        // TODO: ipv6
        {
            match FutureResolver::new() {
                Ok(resolver) => {
                    for i in &resolver.query_a(host).await? {
                        return Ok(i.ipv4().into());
                    }
                    Err(Error::ResolutionFailed)
                }
                Err(_) => Err(Error::ResolutionFailed),
            }
        }
    }
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

impl DnsResolverServerState {
    pub fn new(refresh_frequency: Duration) -> Self {
        DnsResolverServerState {
            hosts: Mutex::new(HashMap::new()),
            refresh_frequency,
        }
    }
}

#[async_trait]
impl TaskServerOperation<Name, Option<IpAddr>> for DnsResolverServerState {
    async fn op(&self, query: Name) -> Option<Option<IpAddr>> {
        // we cache the dns result so as to not spam our DNS resolver
        let mut hosts = self.hosts.lock().await;
        let hostname = query.as_str().to_string();
        if let Some(inner) = hosts.get(&hostname) {
            if inner.last_resolution_time.elapsed() < self.refresh_frequency {
                return Some(Some(inner.last_answer));
            }
        }
        match resolve_host(&hostname).await {
            Ok(last_answer) => {
                hosts.insert(
                    hostname,
                    DnsResolverServerInner {
                        last_answer,
                        last_resolution_time: Instant::now(),
                    },
                );
                Some(Some(last_answer))
            }
            Err(_) => None,
        }
    }
}

pub type DnsResolverServer = TaskServer<Name, Option<IpAddr>, DnsResolverServerState>;
pub type DnsResolverClient = TaskClient<Name, Option<IpAddr>, Error>;
pub type DnsResolverClientService =
    TaskClientService<Name, Option<IpAddr>, std::iter::Once<IpAddr>, Error>;

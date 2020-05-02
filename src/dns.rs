use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::net::IpAddr;

use c_ares_resolver::BlockingResolver;

use crate::errors::Error;

fn resolve_host(host: &str) -> Result<IpAddr, Error> {
	match host.parse() {
		Ok(x) => Ok(x),
		Err(_) =>
			// TODO: ipv6
			match BlockingResolver::new() {
				Ok(resolver) => {
					for i in &resolver.query_a(host)? {
						return Ok(i.ipv4().into());
					}
					Err(Error::DnsResolutionFailed)
				}
				Err(_) => Err(Error::DnsResolutionFailed)
			}
	}
}

#[derive(Clone)]
pub struct DnsResolverServerInner {
	last_resolution_time: Instant,
	last_answer: IpAddr
}

impl DnsResolverServerInner {
	fn run(mut self) {

	}

}

// garbage collection of hosts is ensured at reloading (reloading generates a new
// DnsResolverServer), and restart resolving from scratch
pub struct DnsResolverServerState {
	hosts: HashMap<String, DnsResolverServerInner>,
	refresh_frequency: Duration,

}

impl DnsResolverServerState {
	pub fn new(refresh_frequency: Duration) -> Self {
		DnsResolverServerState {
			hosts: HashMap::new(),
			refresh_frequency
		}
	}
}
/*
impl TaskServerOperation<String, Option<IpAddr>> for DnsResolverServerState {
	fn op(&mut self, query: String) -> Option<Option<IpAddr>> {
		// we cache the dns result so as to not spam our DNS resolver
		let hostname = query.as_str().to_string();
		if let Some(inner) = self.hosts.get(&hostname) {
			if inner.last_resolution_time.elapsed() < self.refresh_frequency {
				return Some(Some(inner.last_answer));
			}
		}
		match resolve_host(&hostname).await {
			Ok(last_answer) => {
				hosts.insert(hostname, DnsResolverServerInner {
					last_answer,
					last_resolution_time: Instant::now()
				});
				Some(Some(last_answer))
			}, Err(_) => {
				None
			}
		}
	}
}

pub type DnsResolverServer = TaskServer<String, Option<IpAddr>, DnsResolverServerState>;
pub type DnsResolverClient = TaskClient<String, Option<IpAddr>, Error>;
*/

use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;

use hyper::client::connect::dns::Name;
use tokio::sync::mpsc::{channel, Sender, Receiver};
use tokio::sync::Mutex;
use c_ares_resolver::FutureResolver;

use crate::errors::Error;
use crate::task::{TaskClient, TaskClientService, select_ok};

async fn resolve_host(host: &str) -> Result<IpAddr, Error> {
	match host.parse() {
		Ok(x) => Ok(x),
		Err(_) =>
			// TODO: ipv6
			match FutureResolver::new() {
				Ok(resolver) => {
					for i in &resolver.query_a(host).await? {
						return Ok(i.ipv4().into());
					}
					Err(Error::ResolutionFailed)
				}
				Err(_) => Err(Error::ResolutionFailed)
			}
	}
}

#[derive(Clone)]
pub struct DnsResolverInner {
	last_resolution_time: Instant,
	last_answer: IpAddr
}

// garbage collection of hosts is ensured at reloading (reloading generates a new
// DnsResolverServer), and restart resolving from scratch
pub struct DnsResolverServer {
	hosts: Mutex<HashMap<String, DnsResolverInner>>,
	refresh_frequency: Duration,
	// TODO: one mutex per client can lead to some significant memory overhead
	clients: Vec<Mutex<(Receiver<Name>, Sender<Option<IpAddr>>)>>
}

impl DnsResolverServer {
	pub fn new(refresh_frequency: Duration) -> Self {
		DnsResolverServer {
			hosts: Mutex::new(HashMap::new()),
			refresh_frequency,
			clients: Vec::new()
		}
	}

	pub fn add_client(&mut self) -> (Sender<Name>, Receiver<Option<IpAddr>>) {
		let (tx_name, rx_name) = channel(25);
		let (tx_res, rx_res) = channel(25);
		self.clients.push(Mutex::new((rx_name, tx_res)));
		(tx_name, rx_res)
	}

	fn gen_waiter<'a>(&'a self, i: usize) -> impl Future<Output = Result<(Name, usize), ()>> + 'a + Send {
		let client = self.clients[i].lock();
		async move {
			let mut client = client.await;
			match client.0.recv().await {
				Some(val) => Ok((val, i)),
				None => Err(())
			}
		}
	}

	async fn resolve(&self, host: Name, i: usize) {
		// we cache the dns result so as to not spam our DNS resolver
		let mut hosts = self.hosts.lock().await;
		if let Some(inner) = hosts.get(host.as_str().into()) {
			if inner.last_resolution_time.elapsed() < self.refresh_frequency {
				let mut client = self.clients.get(i).unwrap().lock().await;
				client.1.send(Some(inner.last_answer)).await.unwrap();
				return;
			}
		}
		match resolve_host(host.as_str()).await {
			Ok(last_answer) => {
				hosts.insert(host.as_str().to_string(), DnsResolverInner {
					last_answer,
					last_resolution_time: Instant::now()
				});
				let mut client = self.clients.get(i).unwrap().lock().await;
				client.1.send(Some(last_answer)).await.unwrap();
			}, Err(_) => {
				let mut client = self.clients.get(i).unwrap().lock().await;
				client.1.send(None).await.unwrap();
			}
		}
	}

	pub async fn run(self) {
		if self.clients.len() == 0 {
			eprintln!("The DnsResolver service was started with 0 clients, are you checking any service in your config file ?");
			return;
		}

		// all thoses allocations probably have a big overhead but are only
		// executed once per load
		let mut v: HashMap<usize, Pin<Box<dyn Future<Output = Result<(Name, usize), ()>> + Send>>> = HashMap::with_capacity(self.clients.len());
		for i in 0..self.clients.len() {
			v.insert(i, Box::pin(self.gen_waiter(i)));
		}

		let mut awaiting_future = select_ok(v);

		loop {
			match awaiting_future.await {
				Ok(((msg, idx), mut rest)) => {
					// re-add a waiter for this element
					rest.insert(idx, Box::pin(self.gen_waiter(idx)));
					awaiting_future = select_ok(rest);

					self.resolve(msg, idx).await;
				}, Err(()) => {
					// return when all channels are closed
					return;
				}
			}
		}
	}
}


pub type DnsResolverClient = TaskClient<Name, Option<IpAddr>, Error>;
pub type DnsResolverClientService = TaskClientService<Name, Option<IpAddr>, std::iter::Once<IpAddr>, Error>;

use std::time::{Instant, Duration};
use std::task::{Context, Poll};
use std::future::Future;
use std::collections::HashMap;
use std::pin::Pin;
use std::net::IpAddr;
use std::sync::Arc;
use std::ops::DerefMut;

use c_ares_resolver::FutureResolver;
use async_trait::async_trait;
use hyper::client::{Client, HttpConnector, connect::dns::Name};
use hyper::service::Service;
use hyper::{Body, Request};
use hyper::body::HttpBody;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use native_tls::TlsConnector;
use hyper_tls::HttpsConnector;
use futures_core::TryFuture;
use futures::future::TryFutureExt;

use crate::metrics::{MetricResult, MetricData};
use crate::errors::Error;

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
// TODO: stop old DnsResolverServer upon reloading
pub struct DnsResolverServer {
	hosts: Mutex<HashMap<String, DnsResolverInner>>,
	refresh_frequency: Duration,
	// TODO: one mutex per clientcan lead to some significant memory overhead
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
					return;
				}
			}
		}
	}
}

// a slightly modified version of the select_ok() method provided by futures_util
pub struct SelectOk<Fut> {
	inner: HashMap<usize, Fut>
}

impl<Fut: Unpin> Unpin for SelectOk<Fut> {}

pub fn select_ok<F>(v: HashMap<usize, F>) -> SelectOk<F>
	where F: TryFuture + Unpin {

	SelectOk {
		inner: v
	}
}

impl<Fut: TryFuture + Unpin> Future for SelectOk<Fut> {
	type Output = Result<(Fut::Ok, HashMap<usize, Fut>), Fut::Error>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		loop {
			let item = self.inner.iter_mut().find_map(|(i, f)| {
				match (*f).try_poll_unpin(cx) {
					Poll::Pending => None,
					Poll::Ready(e) => Some((i, e)),
				}
			});
			match item {
				Some((idx, res)) => {
					let idx = *idx;
					self.inner.remove(&idx);
					match res {
						Ok(e) => {
							let inner = std::mem::replace(&mut self.inner, HashMap::new());
							return Poll::Ready(Ok((e, inner)))
						}
						Err(e) => {
							if self.inner.is_empty() {
								return Poll::Ready(Err(e))
							}
						}
					}
				}
				None => {
					// based on the filter above, nothing is ready, return
					return Poll::Pending
				}
			}
		}
	}
}

#[derive(Clone)]
pub struct DnsResolverClient {
	sender: Arc<std::sync::Mutex<Sender<Name>>>,
	receiver: Arc<std::sync::Mutex<Receiver<Option<IpAddr>>>>
}


impl Service<Name> for DnsResolverClient {
	type Response = std::iter::Once<IpAddr>;
	type Error = Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, name: Name) -> Self::Future {
		// terrible hack due to the fact we cannot have a 'static lifetime here
		// we totally assume that the DnsResolverClient cannot disappear while the client is
		// operating
		let (sender, receiver) = unsafe {
			((self.sender.lock().unwrap().deref_mut() as *mut Sender<Name>).as_mut().unwrap(),
			(self.receiver.lock().unwrap().deref_mut() as *mut Receiver<Option<IpAddr>>).as_mut().unwrap())
		};

		Box::pin(async move {
			let send = sender.send(name).await?;
			let recv = receiver.recv();
			match recv.await.unwrap_or(None).map(|x| std::iter::once(x)) {
				Some(x) => Ok(x),
				None => Err(Error::ResolutionFailed)
			}
		})
	}
}

#[async_trait]
pub trait UrlQueryMaker {
	type R;
	type E;
	async fn internal_query(&self) -> Result<Self::R, Self::E>;
	fn internal_gen_resolver(&self, sender: Sender<Name>, receiver: Receiver<Option<IpAddr>>) -> DnsResolverClient;
	fn internal_gen_client(&mut self, resolver: DnsResolverClient);
}

#[async_trait]
pub trait Url {
	async fn query(&self) -> MetricResult;
	fn gen_client(&mut self, sender: Sender<Name>, receiver: Receiver<Option<IpAddr>>);
}

#[async_trait]
impl<T> Url for HttpWrapper<T>
	where Self: UrlQueryMaker + Send + Sync,
	Result<<Self as UrlQueryMaker>::R, <Self as UrlQueryMaker>::E>: Into<MetricResult> {

	async fn query(&self) -> MetricResult {
		// this is far from ideal (we could be preempted after getting an answer and before reading
		// the time) but atohttpc (and the likes) doesn't expose timing information so we have to restrict ourselves
		// to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
		let start = Instant::now();

		let output: Result<<Self as UrlQueryMaker>::R, <Self as UrlQueryMaker>::E> = self.internal_query().await;

		let stop = start.elapsed();

		output.into().map_success(|x| x.response_time = Some(stop))
	}

	fn gen_client(&mut self,  sender: Sender<Name>, receiver: Receiver<Option<IpAddr>>) {
		self.internal_gen_client(self.internal_gen_resolver(sender, receiver));
	}
}

pub struct HttpWrapper<T> {
	inner: T,
	client: Option<Mutex<Client<HttpsConnector<HttpConnector<DnsResolverClient>>, Body>>>
}

impl<T: std::cmp::Eq + std::hash::Hash> std::hash::Hash for HttpWrapper<T> {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.inner.hash(state);
	}
}

impl<T: std::cmp::Eq> PartialEq for HttpWrapper<T> {
	fn eq(&self, other: &HttpWrapper<T>) -> bool {
		self.inner == other.inner
	}
}

impl<T: std::cmp::Eq> std::cmp::Eq for HttpWrapper<T> {}

impl<T> HttpWrapper<T>
	where HttpWrapper<T>: UrlQueryMaker + Url + PartialEq + Eq + std::hash::Hash {

	pub fn new(v: T) -> Self {
		HttpWrapper {
			inner: v,
			client: None
		}
	}
}


#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HttpStruct {
	pub query: String
}

async fn do_http_query(client: &mut Client<HttpsConnector<HttpConnector<DnsResolverClient>>>, req: Request<Body>) -> Result<(hyper::Response<Body>, u64), Error> {
	let mut res = client.request(req).await?;

	let mut size = 0;
	while let Some(next) = res.data().await {
		size += next?.len();
	}

	Ok((res, size as u64))
}

#[async_trait]
impl UrlQueryMaker for HttpWrapper<HttpStruct> {
	type R = (hyper::Response<Body>, u64);
	type E = Error;

	async fn internal_query(&self) -> Result<Self::R, Self::E> {
		let req = Request::get(&self.inner.query.parse::<http::Uri>()?)
			.header("User-Agent", "monitoring/availcheck")
			.body(Body::empty())?;
		let mut client = self.client.as_ref().unwrap().lock().await;
		let timeout = tokio::time::timeout(Duration::new(7, 0), do_http_query(&mut client, req));
		match timeout.await {
			Ok(Ok(x)) => Ok(x),
			Ok(Err(e)) => Err(e.into()),
			Err(e) => Err(e.into())
		}
	}

	fn internal_gen_resolver(&self, sender: Sender<Name>, receiver: Receiver<Option<IpAddr>>) -> DnsResolverClient {
		DnsResolverClient {
			sender: Arc::new(std::sync::Mutex::new(sender)),
			receiver: Arc::new(std::sync::Mutex::new(receiver))
		}
	}

	fn internal_gen_client(&mut self, resolver: DnsResolverClient) {
		let tls_connector = TlsConnector::new().unwrap();
		let mut http_connector = HttpConnector::new_with_resolver(resolver);
		http_connector.enforce_http(false);
		let https_connector = HttpsConnector::from((http_connector, tls_connector.into()));
		self.client = Some(Mutex::new(hyper::Client::builder()
			.build(https_connector)));
	}
}

impl Into<MetricResult> for Result<(hyper::Response<Body>, u64), Error> {
	fn into(self: Result<(hyper::Response<Body>, u64), Error>) -> MetricResult {
		match self {
			Ok(res) => MetricResult::Success(res.into()),
			Err(e) => {
				if let Error::Timeout(_) = e {
					MetricResult::Timeout
				} else {
					MetricResult::Error
				}
			}
		}
	}
}

impl Into<MetricData> for (hyper::Response<Body>, u64) {
	fn into(self: (hyper::Response<Body>, u64)) -> MetricData {
		MetricData {
			status_code: Some(self.0.status().as_u16()),
			response_time: None,
			response_size: Some(self.1)
		}
	}
}

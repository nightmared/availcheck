use std::time::{Instant, Duration};
use std::task::{Context, Poll};
use std::future::Future;
use std::pin::Pin;
use std::net::IpAddr;

use c_ares_resolver::FutureResolver;
use async_trait::async_trait;
use hyper::client::{Client, HttpConnector, connect::dns::Name};
use hyper::service::Service;
use hyper::{Body, Request};
use hyper::body::HttpBody;
use tokio::sync::Mutex;

use crate::metrics::{MetricResult, MetricData};
use crate::errors::Error;

async fn resolve_host(host: &str) -> Option<IpAddr> {
	match host.parse() {
		Ok(x) => Ok(Some(x)),
		Err(_) =>
			// TODO: ipv6
			match FutureResolver::new() {
				Ok(resolver) =>
					resolver.query_a(host)
						.await
						.map(|res| res.iter().nth(1).map(|x| x.ipv4().into()))
						.map_err(|_| ()),
				Err(_) => Err(())
			}
	}.unwrap_or(None)
}

#[derive(Clone)]
pub struct DnsResolver {
	host: String,
	last_resolution_time: Option<Instant>,
	refresh_frequency: Duration,
	last_answer: Option<IpAddr>
}

impl Service<Name> for DnsResolver {
	type Response = std::option::IntoIter<IpAddr>;
	type Error = std::io::Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
    }

	fn call(&mut self, name: Name) -> Self::Future {
		// some safety check
		assert!(name.as_str() == self.host);
		Box::pin(async move {
			// we cache the dns result so as to not spam our DNS resolver
			// TODO: add caching at this layer
			Ok(resolve_host(name.as_str()).await.into_iter())
			/*if let Some(time) = self.last_resolution_time {
			if time.elapsed() < self.refresh_frequency {
				return Poll::Ready(());
			}
			self.future.poll()
			*/

		})
	}
}

#[async_trait]
pub trait UrlQueryMaker {
	type R;
	type E;
	async fn internal_query(&self) -> Result<Self::R, Self::E>;
	fn gen_resolver(&self, refresh_frequency: Duration) -> DnsResolver;
	fn gen_client(&mut self, resolver: DnsResolver);
}

#[async_trait]
pub trait Url {
	async fn query(&self) -> MetricResult;
}

#[async_trait]
impl<T> Url for UrlWrapper<T>
	where Self: UrlQueryMaker,
	T: Send + Sync,
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
}

pub struct UrlWrapper<T> {
	inner: T,
	client: Option<Mutex<Client<HttpConnector<DnsResolver>, Body>>>
}

impl<T: std::cmp::Eq + std::hash::Hash> std::hash::Hash for UrlWrapper<T> {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.inner.hash(state);
	}
}

impl<T: std::cmp::Eq> PartialEq for UrlWrapper<T> {
	fn eq(&self, other: &UrlWrapper<T>) -> bool {
		self.inner == other.inner
	}
}

impl<T: std::cmp::Eq> std::cmp::Eq for UrlWrapper<T> {}

impl<T> UrlWrapper<T>
	where UrlWrapper<T>: UrlQueryMaker + Url + PartialEq + Eq + std::hash::Hash {

	pub fn new(v: T, dns_refresh_time: u64) -> Self {
		let mut res = UrlWrapper {
			inner: v,
			client: None
		};
		res.gen_client(res.gen_resolver(Duration::new(dns_refresh_time, 0)));
		res
	}
}


#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HttpStruct {
	pub query: String,
	pub tls_enabled: bool,
	pub port: u16,
	pub host: String,
	pub path: String
}

async fn do_query(client: &mut Client<HttpConnector<DnsResolver>, Body>, req: Request<Body>) -> Result<(hyper::Response<Body>, u64), Error> {
	let mut res = client.request(req).await?;

	let mut size = 0;
	while let Some(next) = res.data().await {
        size += next?.len();
    }

	Ok((res, size as u64))
}

#[async_trait]
impl UrlQueryMaker for UrlWrapper<HttpStruct> {
	type R = (hyper::Response<Body>, u64);
	type E = Error;

	async fn internal_query(&self) -> Result<Self::R, Self::E> {
		let req = Request::get(&self.inner.query)
			.header("User-Agent", "monitoring/availcheck")
			.body(Body::empty())
			.unwrap();
		let mut client = self.client.as_ref().unwrap().lock().await;
		let timeout = tokio::time::timeout(Duration::new(7, 0), do_query(&mut client, req));
		match timeout.await {
			Ok(Ok(x)) => Ok(x),
			Ok(Err(e)) => Err(e.into()),
			Err(e) => Err(e.into())
		}
	}

	fn gen_resolver(&self, refresh_frequency: Duration) -> DnsResolver {
		DnsResolver {
			host: self.inner.host.clone(),
			last_resolution_time: None,
			refresh_frequency,
			last_answer: None
		}
	}

	fn gen_client(&mut self, resolver: DnsResolver) {
		self.client = Some(Mutex::new(hyper::Client::builder()
			.build(HttpConnector::new_with_resolver(resolver))));
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

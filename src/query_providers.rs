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
use native_tls::TlsConnector;
use hyper_tls::HttpsConnector;

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

#[derive(Clone)]
pub struct DnsResolver {
	inner: Mutex<Option<DnsResolverInner>>,
	refresh_frequency: Duration,
}

impl Service<Name> for DnsResolver {
	type Response = std::option::IntoIter<IpAddr>;
	type Error = Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
    }

	fn call(&mut self, name: Name) -> Self::Future {
		// some safety check
		Box::pin(async move {
			// we cache the dns result so as to not spam our DNS resolver
			println!("{:?}", resolve_host(name.as_str()).await);
			let inner = self.inner.lock().await;
			if let Some(inner) = *inner {
				if inner.last_resolution_time.elapsed() < self.refresh_frequency {
					return Ok(Some(inner.last_answer).into_iter());
				}
			}
			let last_answer = resolve_host(name.as_str()).await?;
			*inner = Some(DnsResolverInner {
				last_answer,
				last_resolution_time: Instant::now()
			});
			Ok(Some(last_answer).into_iter())
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
}

pub struct HttpWrapper<T> {
	inner: T,
	client: Option<Mutex<Client<HttpsConnector<HttpConnector<DnsResolver>>, Body>>>
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

	pub fn new(v: T, dns_refresh_time: u64) -> Self {
		let mut res = HttpWrapper {
			inner: v,
			client: None
		};
		res.gen_client(res.gen_resolver(Duration::new(dns_refresh_time, 0)));
		res
	}
}


#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HttpStruct {
	pub query: String
}

async fn do_http_query(client: &mut Client<HttpsConnector<HttpConnector<DnsResolver>>>, req: Request<Body>) -> Result<(hyper::Response<Body>, u64), Error> {
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

	fn gen_resolver(&self, refresh_frequency: Duration) -> DnsResolver {
		DnsResolver {
			last_resolution_time: None,
			refresh_frequency,
			last_answer: None
		}
	}

	fn gen_client(&mut self, resolver: DnsResolver) {
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
		println!("{:?}", self);
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

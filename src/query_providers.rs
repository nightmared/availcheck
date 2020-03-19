use std::time::{Instant, Duration};
use std::task::{Context, Poll};
use std::future::Future;
use std::pin::Pin;
use std::borrow::Cow;
use std::net::IpAddr;

use c_ares_resolver::FutureResolver;
use async_trait::async_trait;
use hyper::client::{ResponseFuture, connect::dns::Name};
use hyper::service::{Service, service_fn};
use hyper::{Body, Request};

use crate::metrics::{MetricResult, MetricData};

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
struct DnsResolver<'a> {
	host: &'a str,
	last_resolution_time: Option<Instant>,
	refresh_frequency: Duration,
	last_answer: Option<IpAddr>
}

impl<'a> Service<Name> for DnsResolver<'a> {
	type Response = std::option::IntoIter<IpAddr>;
	type Error = std::io::Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'a>>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
    }

	fn call(&mut self, name: Name) -> Self::Future {
		// some safety check
		assert!(name.as_str() == self.host);
		Box::pin(async {
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
}

#[async_trait]
pub trait Url {
	async fn query<'a>(&self) -> MetricResult;
}

#[async_trait]
impl<T> Url for T
	where T: UrlQueryMaker,
	T: Sync,
	Result<T::R, T::E>: Into<MetricResult> {

	async fn query<'a>(&self) -> MetricResult {
		// this is far from ideal (we could be preempted after getting an answer and before reading
		// the time) but atohttpc (and the likes) doesn't expose timing information so we have to restrict ourselves
		// to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
		let start = Instant::now();

		let output: Result<T::R, T::E> = self.internal_query().await;

		let stop = start.elapsed();

		output.into().map_success(|x| x.response_time = Some(stop))
	}
}

pub struct UrlWrapper<'a, T: 'a> {
	inner: T,
	dns_resolver: DnsResolver<'a>
}

impl<'a, T: std::cmp::Eq + std::hash::Hash> std::hash::Hash for UrlWrapper<'a, T> {
	fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
		self.inner.hash(state);
	}
}

impl<'a, T: std::cmp::Eq> PartialEq for UrlWrapper<'a, T> {
	fn eq(&self, other: &UrlWrapper<'a, T>) -> bool {
		self.inner == other.inner
	}
}

impl<'a, T: std::cmp::Eq> std::cmp::Eq for UrlWrapper<'a, T> {}


impl<'a, T, R, E> UrlWrapper<'a, T> where T: Url + UrlQueryMaker<R=R, E=E> + PartialEq + Eq + std::hash::Hash {
	fn new(v: T, dns_refresh_time: Duration) -> Self {
		UrlWrapper {
			inner: v,
			dns_resolver: v.gen_resolver(dns_refresh_time)
		}
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

#[async_trait]
impl<'a> UrlQueryMaker for UrlWrapper<'a, HttpStruct> {
	type R = hyper::Response<Vec<u8>>;
	type E = hyper::Error;

	async fn internal_query(&'a self) -> Result<Self::R, Self::E> {
		let client = hyper::Client::builder()
			.build(hyper::client::HttpConnector::new_with_resolver(self.dns_resolver));
		let req = Request::get(self.inner.query)
			.header("User-Agent", "monitoring/availcheck")
			.body(Body::empty())
			.unwrap();
		client.request(req).await;
		unimplemented!()
		/*attohttpc::RequestBuilder::try_new(attohttpc::Method::GET, &query)
			.map(|x|
				x.timeout(Duration::new(7, 0))
				.follow_redirects(false)
				.header(attohttpc::header::HeaderName::from_static("host"), attohttpc::header::HeaderValue::from_str(&self.host).unwrap())
			)
			.unwrap()
			.send()*/
	}

	fn gen_resolver(&self, refresh_frequency: Duration) -> DnsResolver {
		DnsResolver {
			host: &self.inner.host,
			last_resolution_time: None,
			refresh_frequency,
			last_answer: None
		}
	}
}

/*
impl Into<MetricResult> for Result<attohttpc::Response, attohttpc::Error> {
	fn into(self: Result<attohttpc::Response, attohttpc::Error>) -> MetricResult {
		match self {
			Ok(res) => MetricResult::Success(MetricData::from(res)),
			Err(e) => {
				if let attohttpc::ErrorKind::Io(e) = e.kind() {
					if e.kind() == std::io::ErrorKind::TimedOut {
						MetricResult::Timeout
					} else {
						MetricResult::Error
					}
				} else {
					MetricResult::Error
				}
			}
		}
	}
}
*/

/*
impl Into<MetricData> for attohttpc::Response {
	fn into(self: attohttpc::Response) -> MetricData {
		MetricData {
			status_code: Some(self.status().as_u16()),
			response_time: None,
			response_size: Some(self.bytes().unwrap().len() as u64)
		}
	}
}
*/

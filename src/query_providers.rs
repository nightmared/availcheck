use std::time::{Instant, Duration};
use std::task::{Context, Poll};
use std::future::Future;
use std::pin::Pin;
use std::net::IpAddr;

use c_ares_resolver::FutureResolver;
use async_trait::async_trait;
use hyper::client::{Client, HttpConnector, ResponseFuture, connect::dns::Name};
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
	fn gen_client(&self, resolver: DnsResolver) -> Client<HttpConnector<DnsResolver>, Body>;
}

#[async_trait]
pub trait Url {
	async fn query(&self) -> MetricResult;
}

#[async_trait]
impl<T> Url for T
	where T: UrlQueryMaker,
	T: Sync,
	Result<T::R, T::E>: Into<MetricResult> {

	async fn query(&self) -> MetricResult {
		// this is far from ideal (we could be preempted after getting an answer and before reading
		// the time) but atohttpc (and the likes) doesn't expose timing information so we have to restrict ourselves
		// to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
		let start = Instant::now();

		let output: Result<T::R, T::E> = self.internal_query().await;

		let stop = start.elapsed();

		output.into().map_success(|x| x.response_time = Some(stop))
	}
}

pub struct UrlWrapper<T> {
	inner: T,
	client: Client<HttpConnector<DnsResolver>, Body>
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

impl<T> UrlWrapper<T> where T: Url + UrlQueryMaker + PartialEq + Eq + std::hash::Hash {
	fn new(v: T, dns_refresh_time: Duration) -> Self {
		let client = v.gen_client(v.gen_resolver(dns_refresh_time));
		UrlWrapper {
			inner: v,
			client
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
impl UrlQueryMaker for UrlWrapper<HttpStruct> {
	type R = hyper::Response<Vec<u8>>;
	type E = hyper::Error;

	async fn internal_query(&self) -> Result<Self::R, Self::E> {
		let req = Request::get(&self.inner.query)
			.header("User-Agent", "monitoring/availcheck")
			.body(Body::empty())
			.unwrap();
		self.client.request(req).await;
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
			host: self.inner.host.clone(),
			last_resolution_time: None,
			refresh_frequency,
			last_answer: None
		}
	}

	fn gen_client(&self, resolver: DnsResolver) -> Client<HttpConnector<DnsResolver>, Body> {
		hyper::Client::builder()
			.build(HttpConnector::new_with_resolver(resolver))
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

use std::time::{Instant, Duration};
use std::borrow::Cow;
use std::net::IpAddr;

use c_ares_resolver::FutureResolver;
use async_trait::async_trait;
use hyper::client::ResponseFuture;

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

#[async_trait]
pub trait UrlQueryMaker {
	type R;
	type E;
	fn internal_query(&self, ip: &Option<IpAddr>) -> Result<Self::R, Self::E>;
	async fn internal_resolve(&self) -> Option<IpAddr>;
}

#[async_trait]
pub trait Url {
	async fn query<'a>(&self, ip: &'a Option<IpAddr>) -> MetricResult;
	async fn resolve(&self) -> Option<IpAddr>;
}

#[async_trait]
impl<T> Url for T
	where T: UrlQueryMaker,
	T: Sync,
	Result<T::R, T::E>: Into<MetricResult> {

	async fn query<'a>(&self, ip: &'a Option<IpAddr>) -> MetricResult {
		// this is far from ideal (we could be preempted after getting an answer and before reading
		// the time) but atohttpc (and the likes) doesn't expose timing information so we have to restrict ourselves
		// to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
		let start = Instant::now();
		let output: Result<T::R, T::E> = self.internal_query(&ip);

		let stop = start.elapsed();

		output.into().map_success(|x| x.response_time = Some(stop))
	}

	async fn resolve(&self) -> Option<IpAddr> {
		self.internal_resolve().await
	}
}


/*
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HttpStruct {
	pub query: String,
	pub tls_enabled: bool,
	pub port: u16,
	pub host: String,
	pub path: String
}

impl UrlQueryMaker for HttpStruct {
	type R = hyper::Response;
	type E = attohttpc::Error;

	async fn internal_query(&self, ip: &Option<IpAddr>) -> Result<Self::R, Self::E> {
		let query = if let Some(ip) = ip {
			Cow::Owned(format!("{}://{}:{}/{}",
				if self.tls_enabled { "https" } else { "http" },
				ip,
				self.port,
				self.path))
		} else {
			Cow::Borrowed(self.query.as_ref())
		};
		hyper::Client::builder()
		/*attohttpc::RequestBuilder::try_new(attohttpc::Method::GET, &query)
			.map(|x|
				x.timeout(Duration::new(7, 0))
				.follow_redirects(false)
				.header(attohttpc::header::HeaderName::from_static("host"), attohttpc::header::HeaderValue::from_str(&self.host).unwrap())
			)
			.unwrap()
			.send()*/
	}

	fn internal_resolve(&self) -> Option<IpAddr> {
		resolve_host(&self.host)
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
*/

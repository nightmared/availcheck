use std::time::{Instant, Duration};
use std::net::IpAddr;

use dns_lookup::lookup_host;

use crate::metrics::MetricResult;

fn resolve_host(host: &str) -> Option<IpAddr> {
	host.parse().map_or_else(|_| {
		lookup_host(host).map(|x| x.get(0).map(|x| *x)).unwrap_or(None)
	}, |x| Some(x))
}

pub trait UrlQueryMaker {
	type R;
	type E;
	fn internal_query(&self, ip: &Option<IpAddr>) -> Result<Self::R, Self::E>;
	fn internal_resolve(&self) -> Option<IpAddr>;
}

pub trait Url {
	fn query(&self, ip: &Option<IpAddr>) -> MetricResult;
	fn resolve(&self) -> Option<IpAddr>;
}

impl<T> Url for T
	where T: UrlQueryMaker,
	Result<T::R, T::E>: Into<MetricResult> {

	fn query(&self, ip: &Option<IpAddr>) -> MetricResult {
		// this is far from ideal (we could be preempted after getting an answer and before reading
		// the time) but atohttpc (and the likes) doesn't expose timing information so we have to restrict ourselves
		// to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
		let start = Instant::now();
		let output: Result<T::R, T::E> = self.internal_query(&ip);
		let stop = start.elapsed();

		output.into().map_success(|x| x.response_time = Some(stop))
	}

	fn resolve(&self) -> Option<IpAddr> {
		self.internal_resolve()
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

impl UrlQueryMaker for HttpStruct {
	type R = attohttpc::Response;
	type E = attohttpc::Error;

	fn internal_query(&self, ip: &Option<IpAddr>) -> Result<Self::R, Self::E> {
		attohttpc::RequestBuilder::try_new(attohttpc::Method::GET, &self.query)
			.map(|x|
				x.timeout(Duration::new(7, 0))
				.header(attohttpc::header::HeaderName::from_static("host"), attohttpc::header::HeaderValue::from_str(&self.host).unwrap())
			)
			.unwrap()
			.send()
	}

	fn internal_resolve(&self) -> Option<IpAddr> {
		resolve_host(&self.host)
	}
}

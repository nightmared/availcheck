use std::time::Instant;
use std::net::IpAddr;

use crossbeam_channel::{Receiver, Sender};
use curl::easy::{Easy, List};

use crate::metrics::MetricResult;
use crate::errors::Error;
//use crate::dns::DnsResolverClient;

pub trait UrlQueryMaker: std::fmt::Debug {
	type R;
	type E;
	fn internal_query(&self) -> Result<Self::R, Self::E>;
}

pub trait Url: std::fmt::Debug {
	fn query(&self) -> MetricResult;
}

#[derive(Debug, Hash, PartialEq, Eq)]
pub struct Client<T> {
	inner: T
}

impl<T> Url for Client<T>
	where Self: UrlQueryMaker + PartialEq + Eq + std::hash::Hash + Send + Sync,
	MetricResult: From<<Self as UrlQueryMaker>::R> {

	fn query(&self) -> MetricResult {
		// this is far from ideal (we could be preempted after getting an answer and before reading
		// the time) but many libraries (and the likes) don't expose timing information so we have to restrict ourselves
		// to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
		let start = Instant::now();

		let output = self.internal_query();

		let stop = start.elapsed();

		match output.map(MetricResult::from) {
			Ok(x) => {
				x.map_success(|x| x.response_time = Some(stop))
			},
			Err(_) => {
				MetricResult::Error
			}
		}
	}
}

impl<T> Client<T>
	where Self: UrlQueryMaker + PartialEq + Eq + std::hash::Hash,
	MetricResult: From<<Self as UrlQueryMaker>::R> {
	
	fn resolve(&self, host: &str) -> Result<IpAddr, Error> {
		unimplemented!()
	}

	pub fn new(v: T) -> Self {
		Client {
			inner: v
		}
	}
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HttpRequest {
	pub query: String,
	pub tls_enabled: bool,
	pub port: u16,
	pub host: String,
	pub path: String
}

impl UrlQueryMaker for Client<HttpRequest> {
	type R = MetricResult;
	type E = Error;

	fn internal_query(&self) -> Result<Self::R, Self::E> {

		unimplemented!()
		/*
		let mut easy = Easy::new();

		easy.url(&self.inner.query)?;

		let ip = self.resolve(&self.inner.host)?.to_string();

		let mut resolve_list = List::new();
		resolve_list.append(format!("{}:{}:{}", self.inner.host, self.inner.port, ip)).unwrap();

		easy.resolve(resolve_list)?;

		let mut size = 0;
		let mut transfer = easy.transfer();
		transfer.write_function(|data| {
			size += data.len();
			Ok(data.len())
		}).unwrap();

		transfer.perform()?;

		let res_code = easy.response_code()?;
		if res_code < 200 || res_code >= 400 {
			return Err(Error::ApiError(APIError {
				url: req.effective_url()?.unwrap_or("<UNKNOWN URL>").into(),
				status_code: res_code,
				body: buf
			}));
		}

		match parse(&buf) {
			Ok(x) => Ok(x),
			Err(e) => Err(e.into())
		}
		*/

			/*
		let req = Request::get(&self.inner.query.parse::<http::Uri>()?)
			.header("User-Agent", "monitoring/availcheck")
			.body(Body::empty())?;
		let mut client = self.client.as_ref().unwrap().lock().await;
		let timeout = tokio::time::timeout(Duration::new(7, 0), do_http_query(&mut client, req));
		let res = timeout.await;
		match res {
			Ok(Ok(x)) => Ok(x),
			Ok(Err(e)) => Err(e.into()),
			Err(e) => Err(e.into())
		}
			*/
	}
}

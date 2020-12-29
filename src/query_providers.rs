use std::net::IpAddr;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hyper::body::HttpBody;
use hyper::client::{connect::dns::Name, Client, HttpConnector};
use hyper::{Body, Request};
use hyper_tls::HttpsConnector;
use native_tls::TlsConnector;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;

use crate::dns::{DnsResolverClient, DnsResolverClientService};
use crate::errors::Error;
use crate::metrics::{MetricData, MetricResult};

#[async_trait]
pub trait UrlQueryMaker {
    type R;
    type E;
    async fn internal_query(&self) -> Result<Self::R, Self::E>;
    fn internal_gen_resolver(
        &self,
        sender: Sender<Name>,
        receiver: Receiver<Option<IpAddr>>,
    ) -> DnsResolverClientService;
    fn internal_gen_client(&mut self, resolver: DnsResolverClientService);
}

#[async_trait]
pub trait Url: std::fmt::Debug {
    async fn query(&self) -> MetricResult;
    fn gen_client(&mut self, sender: Sender<Name>, receiver: Receiver<Option<IpAddr>>);
}

#[async_trait]
impl<T> Url for HttpWrapper<T>
where
    T: std::fmt::Debug,
    Self: UrlQueryMaker + Send + Sync,
    Result<<Self as UrlQueryMaker>::R, <Self as UrlQueryMaker>::E>: Into<MetricResult>,
{
    async fn query(&self) -> MetricResult {
        // this is far from ideal (we could be preempted after getting an answer and before reading
        // the time) but atohttpc (and the likes) doesn't expose timing information so we have to restrict ourselves
        // to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
        let start = Instant::now();

        let output: Result<<Self as UrlQueryMaker>::R, <Self as UrlQueryMaker>::E> =
            self.internal_query().await;

        let stop = start.elapsed();

        output.into().map_success(|x| x.response_time = Some(stop))
    }

    fn gen_client(&mut self, sender: Sender<Name>, receiver: Receiver<Option<IpAddr>>) {
        self.internal_gen_client(self.internal_gen_resolver(sender, receiver));
    }
}

#[derive(Debug)]
pub struct HttpWrapper<T> {
    inner: T,
    client: Option<Mutex<Client<HttpsConnector<HttpConnector<DnsResolverClientService>>, Body>>>,
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
where
    HttpWrapper<T>: UrlQueryMaker + Url + PartialEq + Eq + std::hash::Hash,
{
    pub fn new(v: T) -> Self {
        HttpWrapper {
            inner: v,
            client: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HttpStruct {
    pub query: String,
}

async fn do_http_query(
    client: &mut Client<HttpsConnector<HttpConnector<DnsResolverClientService>>>,
    req: Request<Body>,
) -> Result<(hyper::Response<Body>, u64), Error> {
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
        let res = timeout.await;
        match res {
            Ok(Ok(x)) => Ok(x),
            Ok(Err(e)) => Err(e.into()),
            Err(e) => Err(e.into()),
        }
    }

    fn internal_gen_resolver(
        &self,
        sender: Sender<Name>,
        receiver: Receiver<Option<IpAddr>>,
    ) -> DnsResolverClientService {
        DnsResolverClientService::new(DnsResolverClient::new(sender, receiver), &|x| match x
            .unwrap_or(None)
            .map(|x| std::iter::once(x))
        {
            Some(x) => Ok(x),
            None => Err(Error::ResolutionFailed),
        })
    }

    fn internal_gen_client(&mut self, resolver: DnsResolverClientService) {
        let tls_connector = TlsConnector::new().unwrap();
        let mut http_connector = HttpConnector::new_with_resolver(resolver);
        http_connector.enforce_http(false);
        let https_connector = HttpsConnector::from((http_connector, tls_connector.into()));
        self.client = Some(Mutex::new(hyper::Client::builder().build(https_connector)));
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
            response_size: Some(self.1),
        }
    }
}

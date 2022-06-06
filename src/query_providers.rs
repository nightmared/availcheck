use std::fmt::Debug;
use std::time::Instant;

use async_trait::async_trait;
use hyper::body::HttpBody;
use hyper::client::HttpConnector;
use hyper::{Body, Client, Request};
use hyper_rustls::HttpsConnector;

use crate::dns::DnsService;
use crate::errors::QueryError;
use crate::metrics::{MetricData, MetricResult};

use tracing::{error, instrument, trace};

#[async_trait]
pub trait Url {
    async fn query(&self, resolver: DnsService) -> MetricResult;
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HttpStruct {
    pub query_url: url::Url,
}

#[async_trait]
impl<T> Url for T
where
    T: UrlQueryMaker + Send + Sync + Debug,
    MetricResult: From<Result<<Self as UrlQueryMaker>::R, QueryError>>,
{
    #[instrument]
    async fn query(&self, resolver: DnsService) -> MetricResult {
        // this is far from ideal (we could be preempted after getting an answer and before reading
        // the time) but atohttpc (and the likes) doesn't expose timing information so we have to restrict ourselves
        // to a "simple" monotonic clock. But hey, at least we aren't using gettimeofday() ;)
        let start = Instant::now();

        let output = self.query(resolver).await;

        let stop = start.elapsed();

        MetricResult::from(output).map_success(|x| x.response_time = Some(stop))
    }
}

#[instrument]
async fn do_http_query(
    client: &mut Client<HttpsConnector<HttpConnector<DnsService>>>,
    req: Request<Body>,
) -> Result<(hyper::Response<Body>, u64), QueryError> {
    let mut res = client.request(req).await?;

    let mut size = 0;
    while let Some(next) = res.data().await {
        size += next?.len();
    }

    Ok((res, size as u64))
}

#[async_trait]
pub trait UrlQueryMaker {
    type R;

    async fn query(&self, resolver: DnsService) -> Result<Self::R, QueryError>;
}

#[async_trait]
impl UrlQueryMaker for HttpStruct {
    type R = (hyper::Response<Body>, u64);

    #[instrument]
    async fn query(&self, resolver: DnsService) -> Result<Self::R, QueryError> {
        let mut http_connector = HttpConnector::new_with_resolver(resolver);
        http_connector.enforce_http(false);

        let mut tls_config = rustls::ClientConfig::new();
        tls_config
            .root_store
            .add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
        tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        let https_connector = HttpsConnector::from((http_connector, tls_config));
        let mut client: Client<HttpsConnector<HttpConnector<DnsService>>> =
            Client::builder().build(https_connector);

        let req = Request::get(self.query_url.as_str().parse::<http::Uri>()?)
            .header("User-Agent", "monitoring/availcheck")
            .body(Body::empty())?;
        trace!("query start: {:?}", self);
        let res = do_http_query(&mut client, req).await;
        if let Err(ref e) = res {
            error!(
                "Error performing a query for target {:?}: {:?}",
                self.query_url, e
            );
        }
        res
    }
}

impl From<Result<(hyper::Response<Body>, u64), QueryError>> for MetricResult {
    fn from(val: Result<(hyper::Response<Body>, u64), QueryError>) -> MetricResult {
        match val {
            Ok(res) => MetricResult::Success(res.into()),
            Err(QueryError::Timeout(_)) => MetricResult::Timeout,
            _ => MetricResult::Error,
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

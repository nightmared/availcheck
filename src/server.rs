use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    task::{Context, Poll},
};

use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{sleep, timeout};

use crate::config::Config;
use crate::dns::ExternalDnsResolverPoller;
use crate::errors::ServerError;
use crate::gen_metrics;
use crate::metrics::MetricResult;
use crate::task::select_ready;
use crate::{
    config::{load_config, GlobalConfig, Website},
    errors::QueryError,
};
use crate::{dns::DnsService, errors::MainLoopError};

#[derive(Debug)]
pub struct ServerReload;

pub(crate) struct QueriersServer {
    websites_list: Arc<RwLock<HashSet<Arc<Website>>>>,
    values: HashMap<Arc<Website>, MetricResult>,
    old_values: Arc<RwLock<HashMap<Arc<Website>, MetricResult>>>,
    resolver: DnsService,
}

impl QueriersServer {
    pub fn new(
        resolver: DnsService,
        websites_list: Arc<RwLock<HashSet<Arc<Website>>>>,
    ) -> (Self, Arc<RwLock<HashMap<Arc<Website>, MetricResult>>>) {
        let old_values = Arc::new(RwLock::new(HashMap::new()));

        (
            QueriersServer {
                websites_list,
                values: HashMap::new(),
                old_values: old_values.clone(),
                resolver,
            },
            old_values,
        )
    }

    pub async fn run(&mut self, mut reload_rx: Receiver<()>) {
        let mut queriers: HashMap<Arc<Website>, Pin<Box<dyn Future<Output = MetricResult>>>> =
            HashMap::new();

        let gen_website_querier = |ws: Arc<Website>, resolver: DnsService| async move {
            timeout(
                Duration::new(ws.check_time_seconds, 0),
                ws.url.query(resolver),
            )
            .await
            .unwrap_or(MetricResult::Timeout)
        };

        loop {
            // TODO: use a ticker instead of this shitty code
            //let interval = tokio::time::interval(Duration::new(ws.check_time_seconds, 0));
            tokio::select! {
                // poll the websites loopers
                msg = select_ready(&mut queriers) => match msg {
                    Some((result, website)) => {
                        // update the state
                        self.values.insert(website.clone(), result);

                        // reinsert the website
                        queriers.insert(website.clone(), Box::pin(gen_website_querier(website, self.resolver.clone())));
                    },
                    None => {}
                },
                Some(()) = reload_rx.recv() => {
                    let websites_list = self.websites_list.read().await;
                    for website in websites_list.iter() {
                        if !queriers.contains_key(website) {
                            println!("Target '{}' added.", website.name);
                            queriers.insert(
                                website.clone(),
                                Box::pin(gen_website_querier(website.clone(), self.resolver.clone())),
                            );
                        }
                    }
                    let mut deletions = Vec::new();
                    for querier in queriers.keys() {
                        // the website was deleted
                        if !websites_list.contains(querier) {
                            println!("Target '{}' deleted.", querier.name);
                            deletions.push(querier.clone());
                        }
                    }
                    for d in deletions {
                        queriers.remove(&d);
                    }
                },
            }
            // TODO: improve this update to run only every min(ws.check_time_seconds | ws \in
            // websites)
            *self.old_values.write().await = self.values.clone();
        }
    }
}

pub(crate) struct WebServerState {
    values: Arc<RwLock<HashMap<Arc<Website>, MetricResult>>>,
    reload_chan: Mutex<Sender<ServerReload>>,
}

impl WebServerState {
    async fn ask_config_reloading(self: Arc<Self>) -> Result<&'static str, ServerError> {
        let tx = self.reload_chan.lock().await.clone();
        tx.send(ServerReload {}).await?;
        Ok("Server reloading requested, check the server logs")
    }

    async fn get_results(self: Arc<Self>) -> Result<String, ServerError> {
        let values = self.values.read().await;
        Ok(gen_metrics(&values))
    }
}

async fn web_server(
    listen_addr: IpAddr,
    listen_port: u16,
    state: Arc<WebServerState>,
) -> (impl Future<Output = Result<(), QueryError>>, Sender<()>) {
    let (shutdown_tx, mut shutdown_rx) = channel(2);

    let server_fut = hyper::Server::bind(&SocketAddr::from((listen_addr, listen_port)))
        .serve(hyper::service::make_service_fn(move |_| {
            let state = state.clone();
            async {
                Ok::<_, hyper::Error>(hyper::service::service_fn(move |req| {
                    let state = state.clone();
                    async move {
                        let res: Result<String, ServerError> = match (req.method(), req.uri().path()) {
                                (&http::Method::GET, "/metrics") => state
                                    .get_results()
                                    .await
                                ,
                                // It is crucial to require the client to perform POST request to update the state
                                // to prevent caching and to respect the fact that GET requests MUST be idempotent.
                                //
                                // Important note: You MUST protect this endpoint, either by exposing the server only to
                                // local adresses, or by adding a rule to your config if you have a
                                // web server/load balancer in front on top of this server,
                                // otherwise you risk a DoS if someone call this (very) frequently!
                                (&http::Method::POST, "/server-reload") => {
                                    state.ask_config_reloading().await.map(|x| x.to_string())
                                },
                                _ => Ok("Get metrics at /metrics".to_string()),
                            };
                        Ok::<_, hyper::Error>(
                            res.map(|x| hyper::Response::new(hyper::Body::from(x)))
                                .unwrap_or_else(|e| {
                                    hyper::Response::builder()
                                        .status(500)
                                        .body(hyper::Body::from(e.to_string()))
                                        .unwrap()
                                }),
                        )
                    }
                }))
            }
        }))
        .with_graceful_shutdown(async move { shutdown_rx.recv().await.unwrap() });

    (
        async move { server_fut.await.map_err(|e| e.into()) },
        shutdown_tx,
    )
}

pub(crate) struct App {
    conf: Arc<GlobalConfig>,
    websites_list: Arc<RwLock<HashSet<Arc<Website>>>>,
    state: Arc<WebServerState>,
    pub tx_shutdown_webserv: Sender<()>,
}

impl App {
    pub async fn new(
        config: Config,
    ) -> Result<
        (
            App,
            MainLoopServiceWrapper<'static>,
            MainLoopServiceWrapper<'static>,
            QueriersServer,
            Receiver<ServerReload>,
        ),
        ServerError,
    > {
        let (tx_webserv, rx_webserv) = channel(10);

        let (dns_poller, dns_service) = ExternalDnsResolverPoller::new(Duration::new(
            config.global.dns_refresh_time_seconds,
            0,
        ));

        let websites_list = Arc::new(RwLock::new(config.websites));

        let (queriers, values) = QueriersServer::new(dns_service, websites_list.clone());

        let webserv_state = Arc::new(WebServerState {
            values,
            reload_chan: Mutex::new(tx_webserv),
        });

        let (webserv_fut, tx_shutdown_webserv) = web_server(
            config.global.listen_addr,
            config.global.listen_port,
            webserv_state.clone(),
        )
        .await;

        let app = App {
            conf: config.global,
            websites_list,
            state: webserv_state,
            tx_shutdown_webserv,
        };

        Ok((
            app,
            MainLoopServiceWrapper::new(webserv_fut),
            MainLoopServiceWrapper::new(dns_poller),
            queriers,
            rx_webserv,
        ))
    }

    pub async fn load_new_config<'a>(
        &mut self,
        config_file: &Path,
    ) -> Result<
        Option<(
            Option<MainLoopServiceWrapper<'a>>,
            MainLoopServiceWrapper<'a>,
        )>,
        ServerError,
    > {
        // Updating the list of websites to check (it should be noted that changing the
        // http listening port or adress is not possible at runtime).
        println!("Server reloading asked, let's see what we can do for you...");
        // reload the config
        let new_config = match load_config(config_file).await {
            Ok(x) => x,
            Err(e) => {
                tracing::error!(
                    config_file = config_file.to_str(),
                    message = "Looks like the config file supplied is invalid, aborting the server reload",
                    internal_error = e.to_string().as_str()
                );
                return Ok(None);
            }
        };

        let (new_dns_poller, new_dns_service) = ExternalDnsResolverPoller::new(Duration::new(
            new_config.global.dns_refresh_time_seconds,
            0,
        ));

        if *new_config.global == *self.conf
            && new_config.websites == *self.websites_list.read().await
        {
            println!("No configuration changes found");
            return Ok(None);
        }

        *self.websites_list.write().await = new_config.websites;
        println!("Server reloading finished successfully.",);

        let mut web_serv_fut = None;
        if new_config.global.listen_port != self.conf.listen_port
            || new_config.global.listen_addr != self.conf.listen_addr
        {
            println!(
                "Listening endpoint set to {}:{}",
                new_config.global.listen_addr, new_config.global.listen_port
            );
            let (fut, sender) = web_server(
                new_config.global.listen_addr,
                new_config.global.listen_port,
                self.state.clone(),
            )
            .await;
            web_serv_fut = Some(fut);
            self.tx_shutdown_webserv = sender;
        }

        println!("Server fully reloaded.");

        self.conf = new_config.global;
        Ok(Some((
            web_serv_fut.map(MainLoopServiceWrapper::new),
            MainLoopServiceWrapper::new(new_dns_poller),
        )))
    }
}

pub(crate) struct MainLoopServiceWrapper<'a>(
    Pin<Box<dyn Future<Output = Result<(), MainLoopError>> + 'a>>,
);

impl<'a> MainLoopServiceWrapper<'a> {
    fn new<F: 'a, C>(fut: F) -> Self
    where
        F: Future<Output = Result<(), C>> + 'a,
        C: Into<MainLoopError>,
    {
        Self(Box::pin(async move { fut.await.map_err(|e| e.into()) }))
    }
}
impl<'a> Future for MainLoopServiceWrapper<'a> {
    type Output = Result<(), MainLoopError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

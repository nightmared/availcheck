use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::sleep;

mod config;
mod dns;
mod errors;
mod metrics;
mod query_providers;
mod task;

use config::{load_config, Config, Website};
use dns::DnsService;
use metrics::MetricResult;
use task::{select_ready, select_vec};

#[derive(Debug)]
struct ServerReload;

// TODO: add logging

fn add_metrics_to_string<T: std::fmt::Display>(
    websites: &HashMap<Arc<Website>, MetricResult>,
    s: &mut String,
    name: &str,
    f: &dyn Fn(&MetricResult) -> Option<T>,
) {
    for ws in websites.keys() {
        match websites.get(ws) {
            None => eprintln!("Something is DEEEPLY wrong here !"),
            Some(e) => {
                if let Some(v) = f(&e) {
                    s.push_str(
                        format!("availcheck_{}{{website=\"{}\"}} {}\n", name, ws.name, v).as_str(),
                    );
                }
            }
        }
    }
}

// TODO: add prometheus help to explain the meaning of the variables
fn gen_metrics(websites: &HashMap<Arc<Website>, MetricResult>) -> String {
    // simple heuristic to reduce pointless allocations
    let mut res = String::with_capacity(websites.keys().len() * 75);

    add_metrics_to_string(websites, &mut res, "errors", &|msg| {
        if let MetricResult::Error = msg {
            Some(1)
        } else {
            Some(0)
        }
    });
    add_metrics_to_string(websites, &mut res, "timeouts", &|msg| {
        if let MetricResult::Timeout = msg {
            Some(1)
        } else {
            Some(0)
        }
    });
    add_metrics_to_string(websites, &mut res, "status_code", &|msg| {
        if let MetricResult::Success(ref data) = msg {
            data.status_code
        } else {
            None
        }
    });
    add_metrics_to_string(websites, &mut res, "response_time_ms", &|msg| {
        if let MetricResult::Success(ref data) = msg {
            data.response_time.map(|x| x.as_millis())
        } else {
            None
        }
    });
    add_metrics_to_string(websites, &mut res, "response_size", &|msg| {
        if let MetricResult::Success(ref data) = msg {
            data.response_size
        } else {
            None
        }
    });

    res
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebsiteAction {
    Add(Arc<Website>),
    Delete(Arc<Website>),
    GetMetrics,
}

pub struct QueriersServer {
    clients: Mutex<Vec<(Receiver<WebsiteAction>, Sender<String>)>>,
    resolver: DnsService,
}

impl QueriersServer {
    pub fn new(resolver: DnsService) -> Self {
        QueriersServer {
            clients: Mutex::new(Vec::new()),
            resolver,
        }
    }

    pub async fn new_client(&self) -> (Sender<WebsiteAction>, Receiver<String>) {
        let (tx_name, rx_name) = channel(25);
        let (tx_res, rx_res) = channel(25);
        self.clients.lock().await.push((rx_name, tx_res));
        (tx_name, rx_res)
    }

    async fn gen_web_waiter(&self, i: usize) -> Result<WebsiteAction, ()> {
        match self.clients.lock().await[i].0.recv().await {
            Some(val) => Ok(val),
            None => Err(()),
        }
    }

    async fn gen_website_querier(
        &self,
        ws: Arc<Website>,
        resolver: dns::DnsService,
    ) -> MetricResult {
        let delay = sleep(Duration::new(ws.check_time_seconds, 0));

        let res = ws.url.query(resolver).await;

        delay.await;
        res
    }

    async fn gen_web_waiters<'a>(
        &'a self,
    ) -> HashMap<usize, Pin<Box<dyn Future<Output = Result<WebsiteAction, ()>> + 'a>>> {
        let web_clients = self.clients.lock().await;
        let mut web_waiters: HashMap<
            usize,
            Pin<Box<dyn Future<Output = Result<WebsiteAction, ()>> + 'a>>,
        > = HashMap::with_capacity(web_clients.len());
        for i in 0..web_clients.len() {
            web_waiters.insert(i, Box::pin(self.gen_web_waiter(i)));
        }
        web_waiters
    }

    pub async fn run(&self) {
        let mut websites_state: HashMap<Arc<Website>, MetricResult> = HashMap::new();

        // all thoses allocations probably have a big overhead but are only
        // executed once per config reload
        let mut web_waiters = self.gen_web_waiters().await;

        let mut queriers: HashMap<Arc<Website>, Pin<Box<dyn Future<Output = MetricResult>>>> =
            HashMap::new();

        loop {
            tokio::select! {
                msg = select_ready(&mut web_waiters) => match msg {
                    Some((Ok(action), idx)) => {
                        match action {
                            WebsiteAction::Add(ws) => {
                                queriers.insert(ws.clone(), Box::pin(self.gen_website_querier(ws, self.resolver.clone())));
                            },
                            WebsiteAction::Delete(ws) => {
                                websites_state.remove(&ws);
                                queriers.remove(&ws);
                            },
                            WebsiteAction::GetMetrics => {
                                let metrics = gen_metrics(&websites_state);
                                let _ = self.clients.lock().await[idx].1.send(metrics).await;
                            }
                        }
                        // re-add a waiter for the associated web element
                        web_waiters.insert(idx, Box::pin(self.gen_web_waiter(idx)));
                    },
                    Some((Err(()), _)) => {
                        // coud not get a message, do nothing, and forget about this client
                    }, None => {
                        // no more web waiters, exiting
                    }
                },
                // poll the websites loopers
                msg = select_ready(&mut queriers) => match msg {
                    Some((result, ws)) => {
                        // update the state
                        websites_state.insert(ws.clone(), result);

                        // reinsert the website
                        queriers.insert(ws.clone(), Box::pin(self.gen_website_querier(ws, self.resolver.clone())));
                    },
                    None => {}
                }
            }
        }
    }
}

struct WebServerState {
    upstream: Mutex<(Sender<WebsiteAction>, Receiver<String>)>,
    reload_chan: Mutex<Sender<ServerReload>>,
}

impl WebServerState {
    async fn ask_config_reloading(self: Arc<Self>) -> anyhow::Result<&'static str> {
        let tx = self.reload_chan.lock().await.clone();
        tx.send(ServerReload {}).await?;
        Ok("Server reloading requested, check the server logs")
    }

    async fn get_results(self: Arc<Self>) -> anyhow::Result<String> {
        let mut upstream = self.upstream.lock().await;
        upstream.0.send(WebsiteAction::GetMetrics).await?;
        upstream
            .1
            .recv()
            .await
            .ok_or(anyhow::Error::from(errors::ChannelError))
    }

    async fn add_website(&self, ws: Arc<Website>) {
        let upstream = self.upstream.lock().await;
        upstream.0.send(WebsiteAction::Add(ws)).await.unwrap();
    }

    async fn del_website(&self, ws: Arc<Website>) {
        let upstream = self.upstream.lock().await;
        upstream.0.send(WebsiteAction::Delete(ws)).await.unwrap();
    }
}

async fn web_server(
    listen_addr: IpAddr,
    listen_port: u16,
    state: Arc<WebServerState>,
) -> (Pin<Box<dyn Future<Output = ()>>>, Sender<()>) {
    let (shutdown_tx, mut shutdown_rx) = channel(2);

    let server_fut = hyper::Server::bind(&SocketAddr::from((listen_addr, listen_port)))
        .serve(hyper::service::make_service_fn(move |_| {
            let state = state.clone();
            async {
                Ok::<_, hyper::Error>(hyper::service::service_fn(move |req| {
                    let state = state.clone();
                    async move {
                        let res: anyhow::Result<String> = match (req.method(), req.uri().path()) {
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
                                // otherwise you risk a DoS if someone call this (very) frequently !
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
        Box::pin(async move { server_fut.await.unwrap() }),
        shutdown_tx,
    )
    /*
        .map(move || state_clone.clone())
        .and_then(|state: Arc<WebServerState>| async {
            state
                .ask_config_reloading()
                .await
                .map_err(|e| warp::reject::custom(WebServerError {}))
        });
    */
}

struct App {
    conf: Config,
    state: Arc<WebServerState>,
    tx_shutdown_webserv: Sender<()>,
}

impl App {
    async fn new() -> anyhow::Result<(
        App,
        Pin<Box<dyn Future<Output = ()>>>,
        Pin<Box<dyn Future<Output = ()>>>,
        QueriersServer,
        Receiver<ServerReload>,
    )> {
        let (tx_webserv, rx_webserv) = channel(10);

        let (config, dns_resolver) = load_config().await?;

        let queriers = QueriersServer::new(dns_resolver.tx);
        let (tx, rx) = queriers.new_client().await;

        let webserv_state = Arc::new(WebServerState {
            upstream: Mutex::new((tx, rx)),
            reload_chan: Mutex::new(tx_webserv),
        });

        let (webserv_fut, tx_shutdown_webserv) = web_server(
            config.global.listen_addr,
            config.global.listen_port,
            webserv_state.clone(),
        )
        .await;

        let app = App {
            conf: config,
            state: webserv_state,
            tx_shutdown_webserv,
        };

        Ok((app, webserv_fut, dns_resolver.poller, queriers, rx_webserv))
    }

    async fn load_new_config<'a>(
        &mut self,
    ) -> anyhow::Result<
        Option<(
            Option<Pin<Box<dyn Future<Output = ()> + 'a>>>,
            Pin<Box<dyn Future<Output = ()> + 'a>>,
        )>,
    > {
        // Updating the list of websites to check (it should be noted that changing the
        // http listening port or adress is not possible at runtime).
        println!("Server reloading asked, let's see what we can do for you...");
        // reload the config
        let (new_config, new_resolver_poller) = match load_config().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!(
                    "Looks like your config file is invalid, aborting the procedure: {}",
                    e
                );
                return Ok(None);
            }
        };

        if new_config == self.conf {
            println!("No configuration changes found");
            return Ok(None);
        }

        if new_config.global != self.conf.global {
            // impact every watchers (in case a variable like "dns_refresh_time" changes)
            // TODO: separate the webserver config from the one impacting the watchers to prevent too
            // many restarts
            for w in &new_config.websites {
                self.state.add_website(w.clone()).await;
            }
        } else {
            // enumerate the websites that should be added/deleted
            let differences = new_config
                .websites
                .symmetric_difference(&self.conf.websites);
            // start/stop watchers accordingly
            let mut changes: usize = 0;
            for x in differences {
                changes += 1;
                if !self.conf.websites.contains(x) {
                    // website x has been added
                    self.state.add_website(x.clone()).await;
                } else {
                    // website x has been deleted
                    self.state.del_website(x.clone()).await;
                }
            }
            println!(
                "Server reloading finished successfully with {} website changes.",
                changes
            );
        };

        let mut web_serv_fut = None;
        if new_config.global.listen_port != self.conf.global.listen_port
            || new_config.global.listen_addr != self.conf.global.listen_addr
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

        self.conf = new_config;

        println!("Server fully reloaded.");
        Ok(Some((web_serv_fut, new_resolver_poller.poller)))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut app, mut webserv_fut, mut dns_resolver_fut, queriers, mut rx_webserv) =
        App::new().await?;

    let mut old_tasks = Vec::new();

    loop {
        tokio::select! {
            _ = &mut webserv_fut => {
                panic!("The webserver just crashed !");
            },
            Some(ServerReload {}) = rx_webserv.recv() => {
                if let Some((web_reload_fut, new_dns_resolver_fut)) =
                    app.load_new_config()
                        .await
                        .unwrap() {
                     // relaunch a new web server is necessary;
                    if let Some(new_webserv_fut) = web_reload_fut {
                        // poll the old web server to graceful completion
                        let _ = app.tx_shutdown_webserv.send(());
                        old_tasks.push(webserv_fut);
                        webserv_fut = new_webserv_fut;
                    }
                    old_tasks.push(dns_resolver_fut);
                    dns_resolver_fut = new_dns_resolver_fut;
                }
            },
            _ = select_vec(&mut old_tasks) => {},
            _ = queriers.run() => {
                panic!("The querier service just crashed !");
            },

        }
    }
}

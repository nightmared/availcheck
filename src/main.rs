use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;

mod config;
mod dns;
mod errors;
mod metrics;
mod query_providers;
mod task;

use config::{load_config, GlobalConfig, Website};
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

pub struct QueriersServer {
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

    pub async fn run(&mut self) {
        let mut queriers: HashMap<Arc<Website>, Pin<Box<dyn Future<Output = MetricResult>>>> =
            HashMap::new();

        let gen_website_querier = |ws: Arc<Website>, resolver: DnsService| async move {
            // TODO: use a ticker instead of this drifting shitty code
            let delay = sleep(Duration::new(ws.check_time_seconds, 0));

            let res = ws.url.query(resolver).await;

            delay.await;
            res
        };

        loop {
            for website in self.websites_list.read().await.iter() {
                if !queriers.contains_key(website) {
                    queriers.insert(
                        website.clone(),
                        Box::pin(gen_website_querier(website.clone(), self.resolver.clone())),
                    );
                }
            }
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
                }
            }
            // TODO: improve this update to run only every min(ws.check_time_seconds | ws \in
            // websites)
            *self.old_values.write().await = self.values.clone();
        }
    }
}

struct WebServerState {
    values: Arc<RwLock<HashMap<Arc<Website>, MetricResult>>>,
    reload_chan: Mutex<Sender<ServerReload>>,
}

impl WebServerState {
    async fn ask_config_reloading(self: Arc<Self>) -> anyhow::Result<&'static str> {
        let tx = self.reload_chan.lock().await.clone();
        tx.send(ServerReload {}).await?;
        Ok("Server reloading requested, check the server logs")
    }

    async fn get_results(self: Arc<Self>) -> anyhow::Result<String> {
        let values = self.values.read().await;
        Ok(gen_metrics(&values))
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
}

struct App {
    conf: Arc<GlobalConfig>,
    websites_list: Arc<RwLock<HashSet<Arc<Website>>>>,
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

        let websites_list = Arc::new(RwLock::new(config.websites));

        let (queriers, values) = QueriersServer::new(dns_resolver.tx, websites_list.clone());

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
        Ok(Some((web_serv_fut, new_resolver_poller.poller)))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut app, mut webserv_fut, mut dns_resolver_fut, mut queriers, mut rx_webserv) =
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

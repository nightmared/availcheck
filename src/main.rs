#![feature(get_mut_unchecked)]

use std::sync::{Arc};
use std::time::Duration;
use std::net::{SocketAddr, IpAddr};
use std::collections::HashMap;
use std::pin::Pin;
use std::future::Future;
use std::cell::RefCell;

use tokio::sync::Mutex;
use tokio::sync::mpsc::{channel, Sender, Receiver};
use tokio::time::delay_for;

use warp::Filter;


mod config;
mod dns;
mod errors;
mod metrics;
mod query_providers;
mod task;
use config::{Config, Website, load_config};
use metrics::{WebServMessage, MetricResult};
use task::select_ready;

// TODO: add logging

fn add_metrics_to_string<T: std::fmt::Display>(websites: &HashMap<Arc<Website>, MetricResult>, s: &mut String, name: &str, f: &dyn Fn(&MetricResult) -> Option<T>) {
	for ws in websites.keys() {
		match websites.get(ws) {
			None => eprintln!("Something is DEEEPLY wrong here !"),
			Some(e) => {
				if let Some(v) = f(&e) {
					s.push_str(
						format!("availcheck_{}{{website=\"{}\"}} {}\n",
						name, ws.name, v)
					.as_str());
				}
			}
		}
	}
}

// TODO: add prometheus help to explain the meaning of the variables
fn gen_metrics(websites: &HashMap<Arc<Website>, MetricResult>) -> String {
	// simple heuristic to reduce pointless allocations
	let mut res = String::with_capacity(websites.keys().len() * 75);

	add_metrics_to_string(websites, &mut res, "errors",
		&|msg| if let MetricResult::Error = msg { Some(1) } else { Some(0) });
	add_metrics_to_string(websites, &mut res, "timeouts",
		&|msg| if let MetricResult::Timeout = msg { Some(1) } else { Some(0) });
	add_metrics_to_string(websites, &mut res, "status_code",
		&|msg| if let MetricResult::Success(ref data) = msg { data.status_code } else { None });
	add_metrics_to_string(websites, &mut res, "response_time_ms",
		&|msg| if let MetricResult::Success(ref data) = msg { data.response_time.map(|x| x.as_millis()) } else { None });
	add_metrics_to_string(websites, &mut res, "response_size",
		&|msg| if let MetricResult::Success(ref data) = msg { data.response_size } else { None });

	res
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebsiteAction {
	Add(Arc<Website>),
	Delete(Arc<Website>),
	GetMetrics
}

pub struct QueriersServer {
	clients: Mutex<Vec<(Receiver<WebsiteAction>, Sender<String>)>>
}

impl QueriersServer {
	pub fn new() -> Self {
		QueriersServer {
			clients: Mutex::new(Vec::new())
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
			None => Err(())
		}
	}

	async fn gen_website_querier(&self, ws: Arc<Website>) -> MetricResult {
		let delay = delay_for(Duration::new(ws.check_time_seconds, 0));

		let res = ws.url.query().await;

		delay.await;
		res
	}

	async fn gen_web_waiters<'a>(&'a self) -> HashMap<usize, Pin<Box<dyn Future<Output = Result<WebsiteAction, ()>> + 'a>>> {
		let web_clients = self.clients.lock().await;
		let mut web_waiters: HashMap<usize, Pin<Box<dyn Future<Output = Result<WebsiteAction, ()>> + 'a>>> = HashMap::with_capacity(web_clients.len());
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

		let mut queriers: HashMap<Arc<Website>, Pin<Box<dyn Future<Output = MetricResult>>>> = HashMap::new();

		loop {
			tokio::select! {
				msg = select_ready(&mut web_waiters) => match msg {
					Some((Ok(action), idx)) => {
						match action {
							WebsiteAction::Add(ws) => {
								queriers.insert(ws.clone(), Box::pin(self.gen_website_querier(ws)));
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
						queriers.insert(ws.clone(), Box::pin(self.gen_website_querier(ws)));
					},
					None => {}
				}
			}
		}
	}
}

struct WebServerState {
	running: Mutex<bool>,
	upstream: Mutex<(Sender<WebsiteAction>, Receiver<String>)>,
	webserver_chan: Mutex<Sender<WebServMessage>>
}

impl WebServerState {
	async fn ask_config_reloading(self: Arc<Self>) -> Result<impl warp::Reply, std::convert::Infallible> {
		let mut tx = self.webserver_chan.lock().await.clone();
		tx.send(WebServMessage::ReloadConfig).await.unwrap();
		Ok("Server reloading requested, check the server logs")
	}

	async fn get_results(self: Arc<Self>) -> Result<impl warp::Reply, std::convert::Infallible> {
		let mut upstream = self.upstream.lock().await;
		upstream.0.send(WebsiteAction::GetMetrics).await.unwrap();
		Ok(upstream.1.recv().await.unwrap())
	}

	async fn add_website(&self, ws: Arc<Website>) {
		let mut upstream = self.upstream.lock().await;
		upstream.0.send(WebsiteAction::Add(ws)).await.unwrap();
	}

	async fn del_website(&self, ws: Arc<Website>) {
		let mut upstream = self.upstream.lock().await;
		upstream.0.send(WebsiteAction::Delete(ws)).await.unwrap();
	}
}


async fn web_server(listen_addr: IpAddr, listen_port: u16, state: Arc<WebServerState>) {
	let state_clone = state.clone();
	let get_metrics = warp::path("metrics")
		.and(warp::path::end())
		.map(move || state.clone())
		.and_then(WebServerState::get_results);

	// It is crucial to require the client to perform POST request to update the state
	// to prevent caching and to respect the fact that GET requests MUST be idempotent.
	//
	// Important note: You MUST protect this endpoint, either by exposing the server only to
	// local adresses, or by adding a rule to your config if you have a
	// web server/load balancer in front on top of this server,
	// otherwise you risk a DoS if someone call this (very) frequently !
	let reload_server = warp::post()
		.and(warp::path("server-reload"))
		.and(warp::path::end())
		.map(move || state_clone.clone())
		.and_then(WebServerState::ask_config_reloading);

	let default_path = warp::any()
		.map(|| "Get metrics at /metrics" );

	let routes = get_metrics.or(reload_server).or(default_path);

	warp::serve(routes).run(SocketAddr::from((listen_addr, listen_port))).await
}

async fn reload_config<'a>(old_config: &Config, state: Arc<WebServerState>) -> std::io::Result<Option<(Config, Option<Pin<Box<impl Future + 'a>>>)>> {
	// Updating the list of websites to check (it should be noted that changing the
	// http listening port or adress is not possible at runtime).
	println!("Server reloading asked, let's see what we can do for you...");
	// reload the config
	let new_config: Config = match load_config().await {
		Ok(x) => x,
		Err(e) => {
			eprintln!("Looks like your config file is invalid, aborting the procedure: {}", e);
			return Ok(None);
		}
	};

	if new_config == *old_config {
		println!("No configuration changes found");
		return Ok(None);
	}

	if new_config.global != old_config.global {
		// impact every watchers (in case a variable like "dns_refresh_time" changes)
		// TODO: separate the webserver config from the one impacting the watchers to prevent too
		// many restarts
		for w in &new_config.websites {
			state.add_website(w.clone()).await;
		}

	} else {
		// enumerate the websites that should be added/deleted
		let differences = new_config.websites.symmetric_difference(&old_config.websites);
		// start/stop watchers accordingly
		let mut changes = 0;
		for x in differences {
			changes += 1;
			if !old_config.websites.contains(x) {
				// website x has been added
				state.add_website(x.clone()).await;
			} else {
				// website x has been deleted
				state.del_website(x.clone()).await;
			}
		}
		println!("Server reloading finished successfully with {} website changes.", changes);
	};

	let mut running = state.running.lock().await;
	let mut web_serv_fut = None;
	if new_config.global.listen_port != old_config.global.listen_port
		|| new_config.global.listen_addr != old_config.global.listen_addr
		|| !*running {
			*running = true;
			drop(running);
			println!("Listening endpoint set to {}:{}", new_config.global.listen_addr, new_config.global.listen_port);
			web_serv_fut = Some(Box::pin(web_server(new_config.global.listen_addr, new_config.global.listen_port, state)));
	}

	println!("Server fully reloaded.");
	Ok(Some((new_config, web_serv_fut)))
}

const WEB_SERV_TASK: i8 = 1;
const QUERIERS_TASK: i8 = 2;
const WEB_SERV_CHANNEL_TASK: i8 = 3;

struct WebServerMessageReceiver {
	rx: RefCell<Receiver<WebServMessage>>
}

impl WebServerMessageReceiver {
	async fn run(&self) -> Option<WebServMessage> {
		match self.rx.borrow_mut().recv().await {
			Some(x) => {
				return Some(x);
			},
			None => {
				eprintln!("Error encountered while reading on the web server channel, exiting...");
				panic!();
			}
		}
	}
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
	let mut config: Config = Config::default();

	// force config reloading at startup
	// (the main advanatge of this is that there is no substantial difference between the initial
	// setup and subsequent refreshes)
	let (mut tx_webserv, rx_webserv) = channel(10);
	tx_webserv.send(WebServMessage::ReloadConfig).await.unwrap();

	let queriers = QueriersServer::new();
	let (tx, rx) = queriers.new_client().await;

	let web_serv_state = Arc::new(WebServerState {
		running: Mutex::new(false),
		upstream: Mutex::new((tx, rx)),
		webserver_chan: Mutex::new(tx_webserv)
	});
	let web_serv_fut = web_server(config.global.listen_addr, config.global.listen_port, web_serv_state.clone());

	let web_serv_message_receiver = WebServerMessageReceiver {
		rx: RefCell::new(rx_webserv)
	};

	let mut waiting_tasks: HashMap<i8, Pin<Box<dyn Future<Output = Option<WebServMessage>>>>> = HashMap::new();
	waiting_tasks.insert(WEB_SERV_TASK, Box::pin(async {
		web_serv_fut.await;
		None
	}));
	waiting_tasks.insert(QUERIERS_TASK, Box::pin(async {
		queriers.run().await;
		None
	}));
	waiting_tasks.insert(WEB_SERV_CHANNEL_TASK, Box::pin(web_serv_message_receiver.run()));

	loop {
		match select_ready(&mut waiting_tasks).await {
			Some((Some(WebServMessage::ReloadConfig), _)) => {
				if let Some((new_config, web_reload_fut)) = reload_config(&config, web_serv_state.clone()).await.unwrap() {
					// relaunch a new web server is necessary;
					if let Some(new_web_serv_fut) = web_reload_fut {
						waiting_tasks.insert(WEB_SERV_TASK, Box::pin(async {
							new_web_serv_fut.await;
							None
						}));
					}
					config = new_config;
				}
				// reinsert itself
				waiting_tasks.insert(WEB_SERV_CHANNEL_TASK, Box::pin(web_serv_message_receiver.run()));
			},
			Some((None, _)) => {
				println!("Some service crashed !");
			},
			None => {
				panic!("Should not happen !");
			}
		}
	}
}

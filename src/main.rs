use std::sync::{Arc};
use std::thread;
use std::time::{Instant, Duration};
use std::sync::Mutex;
use std::net::{SocketAddr, IpAddr};
use std::collections::HashMap;
use std::pin::Pin;
use std::future::Future;
use std::cell::RefCell;
use std::sync::atomic::{Ordering, AtomicBool};

use warp::Filter;

use crossbeam_channel::{select, Select, bounded, Sender, Receiver};

mod blocking_task;
mod config;
mod dns;
mod errors;
mod metrics;
mod query_providers;
mod task;
use blocking_task::{ServerCommand, ServerMessage, TaskWorker, TaskWorkerOperation, TaskServer, TaskServerOperation};
use errors::Error;
use config::{Config, Website, load_config};
use metrics::MetricResult;
use task::PollWhileEnabled;

// TODO: add logging

struct LooperState {
	website: Arc<Website>,
	wait_time: Duration,
	start_time: Instant
}
type WebsiteLooper = TaskWorker<(), MetricResult, LooperState>;

impl LooperState {
	fn new(website: Arc<Website>) -> Self {
		let wait_time = Duration::new(website.check_time_seconds, 0);
		LooperState {
			website,
			wait_time,
			// the counter is already elapsed at startup to perform the first query right away
			// instead of waiting for the timer to timeout
			start_time: Instant::now()-wait_time
		}
	}
}

impl TaskWorkerOperation<(), MetricResult> for LooperState {
	fn op(&mut self, _query: ()) -> Option<MetricResult> {
			// Sleep until it is our time to shine
			// the loop prevents against spurious wakeups
			while self.start_time.elapsed() < self.wait_time {
				thread::sleep(self.wait_time-self.start_time.elapsed());
			}

			Some(self.website.url.query())
	}
}

type WebsiteAction = ServerCommand<Arc<Website>, (), LooperState>;
type WebsiteAnswer = ServerMessage<Arc<Website>>;
type QuerierState = Arc<Mutex<HashMap<Arc<Website>, MetricResult>>>;
type QueriersServer = TaskServer<Arc<Website>, (), MetricResult, QuerierState, LooperState>;


impl TaskServerOperation<Arc<Website>, (), MetricResult> for QuerierState {
	fn op(&mut self, idx: &Arc<Website>, _query: &(), val: MetricResult) {
		self.lock().unwrap().insert(idx.clone(), val);
	}
	fn cleanup(&mut self, idx: &Arc<Website>) {
		self.lock().unwrap().remove(idx);
	}
}

pub struct QueriersServerHandle {
	shared_state: Arc<Mutex<HashMap<Arc<Website>, MetricResult>>>,
	tx: Sender<WebsiteAction>,
	rx: Receiver<WebsiteAnswer>
}

impl QueriersServerHandle {
	fn add_website(&mut self, ws: Arc<Website>) {
		self.tx.send(WebsiteAction::Add((ws.clone(), LooperState::new(ws)))).unwrap();
	}

	fn del_website(&mut self, ws: Arc<Website>) {
		self.tx.send(WebsiteAction::Delete(ws)).unwrap();
	}
}

#[derive(Debug, PartialEq)]
enum WebServerMessage {
	ReloadConfig,
	Bye
}

struct WebServer {
	shared_state: Arc<Mutex<HashMap<Arc<Website>, MetricResult>>>,
	tx: Sender<WebServerMessage>
}

impl WebServer {
	async fn run(self, listen_addr: IpAddr, listen_port: u16) {
		let shared_state = self.shared_state.clone();
		let get_metrics = warp::path("metrics")
			.and(warp::path::end())
			.map(move || metrics::gen_metrics(&shared_state.lock().unwrap()));

		// It is crucial to require the client to perform POST request to update the state
		// to prevent caching and to respect the fact that GET requests MUST be idempotent.
		//
		// Important note: You MUST protect this endpoint, either by exposing the server only to
		// local adresses, or by adding a rule to your config if you have a
		// web server/load balancer in front on top of this server,
		// otherwise you risk a DoS if someone call this (very) frequently !
		let tx = self.tx.clone();
		let reload_server = warp::post()
			.and(warp::path("server-reload"))
			.and(warp::path::end())
			.map(move || {
				tx.send(WebServerMessage::ReloadConfig).unwrap();
				"Reloading server configuration"
			});

		let default_path = warp::any()
			.map(|| "Get metrics at /metrics" );

		unimplemented!()
		//let routes = get_metrics.or(reload_server).or(default_path);

		//println!("Listening endpoint set to {}:{}", listen_addr, listen_port);
		//warp::serve(routes).run(SocketAddr::from((listen_addr, listen_port))).await
	}
}

struct WebServerHandle {
	rx: Receiver<WebServerMessage>,
	enabled: Arc<AtomicBool>
}

fn reload_config(old_config: &mut Config, web_server: Option<WebServerHandle>, queriers: &mut QueriersServerHandle) -> Option<WebServerHandle> {
	// Updating the list of websites to check (it should be noted that changing the
	// http listening port or adress is not possible at runtime).
	println!("Server reloading asked, let's see what we can do for you...");
	// reload the config
	let new_config: Config = match load_config() {
		Ok(x) => x,
		Err(e) => {
			eprintln!("Looks like your config file is invalid, aborting the procedure: {}", e);
			return None;
		}
	};

	if new_config == *old_config {
		println!("No configuration changes found");
		return None;
	}

	if new_config.global != old_config.global {
		// impact every watchers (in case a variable like "dns_refresh_time" changes)
		// TODO: separate the webserver config from the one impacting the watchers to prevent too
		// many restarts
		for w in &new_config.websites {
			queriers.add_website(w.clone());
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
				queriers.add_website(x.clone());
			} else {
				// website x has been deleted
				queriers.del_website(x.clone());
			}
		}
		println!("Server reloading finished successfully with {} website changes.", changes);
	};

	let mut res = None;
	if new_config.global.listen_port != old_config.global.listen_port
		|| new_config.global.listen_addr != old_config.global.listen_addr
		|| web_server.is_none() {
			if let Some(handle) = web_server {
				handle.enabled.store(false, Ordering::Release);
				// wait for the webserver to exit
				while handle.rx.recv().unwrap() != WebServerMessage::Bye {}
			}
			let (tx, rx) = bounded(10);
			let server = WebServer {
				shared_state: queriers.shared_state.clone(),
				tx
			};

			let enabled = Arc::new(AtomicBool::new(true));
			let enabled_clone = enabled.clone();

			res = Some(WebServerHandle {
				rx,
				enabled
			});

			let (addr, port) = (new_config.global.listen_addr, new_config.global.listen_port);
			tokio::spawn(async move {
				PollWhileEnabled {
					enabled: enabled_clone,
					fut: Box::pin(server.run(addr, port))
				}.await
			});
	}

	*old_config = new_config;
	println!("Server fully reloaded.");
	res
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
	let mut config: Config = Config::default();


	let mut queriers = {
		let shared_state = Arc::new(Mutex::new(HashMap::new()));
		let mut queriers = QueriersServer::new(shared_state.clone());
		let (tx, rx) = queriers.gen_client();
		thread::spawn(move || queriers.run());
		QueriersServerHandle {
			shared_state,
			tx,
			rx
		}
	};

	// force config reloading at startup
	// (the main advanatge of this is that there is no substantial difference between the initial
	// setup and subsequent refreshes)
	let mut web_server_handle = reload_config(&mut config, None, &mut queriers)
		.expect("User configuration does not differ from the default configuration");

	loop {
		select! {
			recv(queriers.rx) -> msg => if let Ok(_msg) = msg {
				// TODO: check that actions succeeded
			},
			recv(web_server_handle.rx) -> msg => if let Ok(msg) = msg {
				match msg {
					WebServerMessage::ReloadConfig => {
						web_server_handle = match reload_config(&mut config, None, &mut queriers) {
							Some(x) => x,
							None => web_server_handle
						};
					},
					WebServerMessage::Bye => {
						// TODO

					}
				}
			}

		}
		/*
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
		*/
	}
}

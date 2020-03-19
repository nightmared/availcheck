use std::thread;
use std::sync::{Arc, RwLock};
use std::time::{Instant, Duration};
use std::net::{SocketAddr, IpAddr};
use std::sync::atomic::Ordering;
use std::collections::HashMap;

use tokio::sync::oneshot;

use crossbeam_channel::{unbounded, select, Sender};

use warp::Filter;


mod config;
mod metrics;
mod query_providers;
use config::{Config, GlobalConfig, Website, load_config};
use metrics::{WatcherMessage, WebServMessage, WebsiteMessageType, MetricResult};

// TODO: add logging


async fn loop_website(global_config: Arc<GlobalConfig>, ws: Arc<Website>, send_queue: Sender<WatcherMessage>) {
	let wait_time = Duration::new(ws.check_time_seconds, 0);
	loop {
		let start = Instant::now();
		if !ws.enabled.load(Ordering::Acquire) {
			send_queue.send(WatcherMessage {
				website: ws,
				msg: WebsiteMessageType::Exit
			}).unwrap();
			return;
		}

		let res = ws.url.query().await;

		send_queue.send(WatcherMessage {
			website: ws.clone(),
			msg: WebsiteMessageType::MetricResult(res)
		}).unwrap();
		// the 'while' loop prevents against spurious wakeups
		while start.elapsed() < wait_time {
			thread::sleep(wait_time-start.elapsed());
		}
	}
}

struct ServerState {
	websites: HashMap<Arc<Website>, MetricResult>,
	// the first element is a channel to ask the webserver to shutdown itself, and the second one
	// is a receiver notified when the server is supposedly stopped
	shutdown_web_server_chan: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
	webserver_chan: Sender<WebServMessage>
}

impl ServerState {
	fn new_empty(tx_webserv: Sender<WebServMessage>) -> Arc<RwLock<Self>> {
		Arc::new(RwLock::new(ServerState {
			websites: HashMap::new(),
			shutdown_web_server_chan: None,
			webserver_chan: tx_webserv
		}))
	}

	fn add_metrics_to_string<T: std::fmt::Display>(&self, s: &mut String, name: &str, f: &dyn Fn(&MetricResult) -> Option<T>) {
		for ws in self.websites.keys() {
			match self.websites.get(ws) {
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
	fn delete_website(&mut self, website: &Arc<Website>) {
		self.websites.remove(website);
	}

	fn update_metrics(&mut self, website: &Arc<Website>, value: MetricResult) {
		if self.websites.contains_key(website) {
			*self.websites.get_mut(website).unwrap() = value;
		} else {
			self.websites.insert(website.clone(), value);
		 }
	}
}

// TODO: add prometheus help to explain the meaning of the variables
fn gen_metrics_from_state(state: &ServerState) -> String {
	// simple heuristic to reduce pointless allocations
	let mut res = String::with_capacity(state.websites.keys().len() * 75);

	state.add_metrics_to_string(&mut res, "errors",
		&|msg| if let MetricResult::Error = msg { Some(1) } else { Some(0) });
	state.add_metrics_to_string(&mut res, "timeouts",
		&|msg| if let MetricResult::Timeout = msg { Some(1) } else { Some(0) });
	state.add_metrics_to_string(&mut res, "status_code",
		&|msg| if let MetricResult::Success(ref data) = msg { data.status_code } else { None });
	state.add_metrics_to_string(&mut res, "response_time_ms",
		&|msg| if let MetricResult::Success(ref data) = msg { data.response_time.map(|x| x.as_millis()) } else { None });
	state.add_metrics_to_string(&mut res, "response_size",
		&|msg| if let MetricResult::Success(ref data) = msg { data.response_size } else { None });

	res
}

async fn web_server(listen_addr: IpAddr, listen_port: u16, state: Arc<RwLock<ServerState>>) {
	let tx = state.read().unwrap().webserver_chan.clone();
	let get_metrics = warp::path("metrics")
		.and(warp::path::end())
		.map(move || gen_metrics_from_state(&state.read().unwrap()) );

	let reload_server = warp::post()
		.and(warp::path("server-reload"))
		.and(warp::path::end())
		.map(move || {
			// It is crucial to require the client to perform POST request to update the state
			// to prevent caching and to respect the fact that GET requests MUST be idempotent.
			//
			// Note: You MUST protect this endpoint, either by exposing the server only to
			// local adresses, or by adding a rule to your config if you have a
			// web server/load balancer in front on top of this server,
			// otherwise you risk a DoS if someone call this (very) frequently !
			tx.send(WebServMessage::ReloadConfig).unwrap();
			"Server reloading requested, check the server logs"
		});

	let default_path = warp::any()
		.map(|| "Get metrics at /metrics" );

	let routes = get_metrics.or(reload_server).or(default_path);

	warp::serve(routes).run(SocketAddr::from((listen_addr, listen_port))).await
}

async fn spawn_watcher(global_config: Arc<GlobalConfig>, website: &Arc<Website>, tx: Sender<WatcherMessage>) {
	println!("Watching {}", website.name);
	tokio::spawn(loop_website(global_config.clone(), website.clone(), tx));
}

async fn reload_web_server(addr: IpAddr, port: u16, state: Arc<RwLock<ServerState>>) {
	let (tx_shutdown, rx_shutdown) = oneshot::channel();
	let (tx_exited, rx_exited) = oneshot::channel();

	let mut rx_tx_couple = Some((tx_shutdown, rx_exited));

	std::mem::swap(&mut state.write().unwrap().shutdown_web_server_chan, &mut rx_tx_couple);

	// shutdown the previous webserver
	if let Some((tx, rx)) = rx_tx_couple {
		tx.send(()).unwrap();
		rx.await.unwrap();
	}

	let new_webserv_future = web_server(addr, port, state.clone());

	// hot-reloading the web server is backed by the fact that dropping the web server future
	// will shutdown it (see
	// https://github.com/hyperium/hyper/blob/dd02254ae8c6c59432254a546bee6c9e95e16400/tests/server.rs#L1928-L1940
	// for more info ?). To do so, we send a shutdown signal to the future containing the
	// webserver. That future selects on the both the webserver future and the shutdown channel,
	// and will exit upon such signal, dropping() the webserver on the way.
	tokio::spawn(async move {
		let mut new_webserv_future = Box::pin(new_webserv_future);
		let mut rx_shutdown = Box::pin(rx_shutdown);
		loop {
			tokio::select! {
				_ = &mut new_webserv_future => {},
				_ = &mut rx_shutdown => {
					// drop the previous webserver if any (should stop the web server)
					// TODO: make sure that any ongoing connections are dropped (so that the socket
					// doesn't end up in TIME_WAIT) before send the 'exited' signal
					std::mem::drop(new_webserv_future);
					tx_exited.send(()).unwrap();
					return;
				}
			}
		}
	});
	println!("Listening endpoint set to {}:{}", addr, port);
}

async fn reload_config(old_config: &mut Config, state: Arc<RwLock<ServerState>>, tx_watchers: Sender<WatcherMessage>) -> std::io::Result<()> {
	// Updating the list of websites to check (it should be noted that changing the
	// http listening port or adress is not possible at runtime).
	println!("Server reloading asked, let's see what we can do for you...");
	// reload the config
	let new_config: Config = match load_config() {
		Ok(x) => x,
		Err(e) => {
			eprintln!("Looks like your config file is invalid, aborting the procedure: {}", e);
			return Ok(());
		}
	};


	if new_config.global != old_config.global {
		// disable every watcher and spawn new ones, as the global config changed, which may
		// impact every watchers (in case a variable like "dns_refresh_time" changes)
		// TODO: separate the webserver config from the one impacting the watchers to prevent too
		// many restarts
		for w in &old_config.websites {
			w.enabled.store(false, Ordering::Release);
		}
		for w in &new_config.websites {
			spawn_watcher(new_config.global.clone(), &w, tx_watchers.clone()).await;
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
				spawn_watcher(new_config.global.clone(), x, tx_watchers.clone()).await;
			} else {
				// website x has been deleted
				x.enabled.store(false, Ordering::Release);
			}
		}
		println!("Server reloading finished successfully with {} website changes.", changes);
	};

	if state.read().unwrap().shutdown_web_server_chan.is_none()
		|| new_config.global.listen_port != old_config.global.listen_port
		|| new_config.global.listen_addr != old_config.global.listen_addr {
			reload_web_server(new_config.global.listen_addr, new_config.global.listen_port, state.clone()).await;
	}
	*old_config = new_config;

	println!("Server fully reloaded.");
	Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
	let mut config: Config = Config::default();

	let (tx_watchers, rx_watchers)
		: (crossbeam_channel::Sender<WatcherMessage>, crossbeam_channel::Receiver<WatcherMessage>)
		= unbounded();

	// force config reloading at startup
	// (the main advanatge of this is that there is no substantial difference between the initial
	// setup and subsequent refreshes)
	let (tx_webserv, rx_webserv) = unbounded();
	tx_webserv.send(WebServMessage::ReloadConfig).unwrap();

	let state = ServerState::new_empty(tx_webserv);

	loop {
		select! {
			recv(rx_watchers) -> msg => match msg {
				Ok(msg) => {
					let mut state_wrt = state.write().unwrap();
					// This website was deleted at runtime, let's remove it
					match msg.msg {
						WebsiteMessageType::Exit  => state_wrt.delete_website(&msg.website),
						WebsiteMessageType::MetricResult(e) => state_wrt.update_metrics(&msg.website, e)
					}
				},
				Err(e) => {
					eprintln!("{} error encountered while reading on the watchers channel, exiting...", e);
					panic!();
				}
			},
			recv(rx_webserv) -> msg => match msg {
				Ok(WebServMessage::ReloadConfig) =>
					reload_config(&mut config, state.clone(), tx_watchers.clone()).await?,
				Err(e) => {
					eprintln!("{} error encountered while reading on the web server channel, exiting...", e);
					panic!();
				}
			}
		}
	}
}

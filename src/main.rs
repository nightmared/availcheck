use std::{env, thread};
use std::panic::{self, AssertUnwindSafe};
use std::io::{self, prelude::*};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Instant, Duration};
use std::net::{SocketAddr, IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::{HashSet, HashMap};
use std::fs::File;

use tokio::sync::oneshot;

use crossbeam_channel::{unbounded, select, Sender};

use warp::Filter;

use dns_lookup::lookup_host;

use serde::{Deserialize, Deserializer};
use serde::de;

// TODO: add logging

fn get_base_dir() -> PathBuf {
	let config_base_dir = env::var_os("XDG_CONFIG_HOME")
		.unwrap_or(env::var_os("HOME")
			.unwrap_or(env::current_dir()
				.expect("Couldn't get the current directory")
					.into_os_string()));
	Path::new(&config_base_dir).join("availcheck/")
}

fn load_yaml_file<T: for <'a> Deserialize<'a>>(file_name: &str) -> std::io::Result<T> {
	let mut fs = File::open(file_name).map_err(|e| {
		eprintln!("Unable to open the file '{}'.", file_name);
		e
	})?;
	let mut conf_buf = Vec::new();
	fs.read_to_end(&mut conf_buf)?;
	serde_yaml::from_slice::<T>(&conf_buf).map_err(|e| {
		eprintln!("Unable to parse the file '{}': {:?}", file_name, e);
		io::Error::from(io::ErrorKind::InvalidData)
	})
}

fn load_app_data<T: for <'a> Deserialize<'a>>(file_name: &str) -> std::io::Result<T> {
	let mut config_file_path = get_base_dir();
	config_file_path.push(file_name);
	// if your filename is not a valid utf-8 name, it's YOUR problem (like using a weird path
	// and/or doing weird things).
	// TODO: look into OsStr
	load_yaml_file(&config_file_path.to_string_lossy())
}

// This function only works if you give it a sorted list
fn get_duplicates<'a>(sorted_vec: &'a Vec<&String>) -> HashSet<&'a str> {
	let mut duplicates = HashSet::new();
	for i in 1..sorted_vec.len() {
		if sorted_vec[i-1] == sorted_vec[i] {
			duplicates.insert(sorted_vec[i].as_ref());
		}
	}
	duplicates
}


fn load_config() -> std::io::Result<Config> {
	let conf = load_app_data("config.yml");

	conf.map(|conf: SerializedConfig| {
		let mut names = conf.websites
			.iter()
			.map(|x| &x.name)
			.collect::<Vec<&String>>();
		names.sort();
		let duplicates = get_duplicates(&names);
		if duplicates.len() != 0 {
			return Err(io::Error::new(io::ErrorKind::InvalidData,
				format!("Duplicate names in your config file: {:?}", duplicates)));
		}

		Ok(Config {
			websites: conf.websites
				.into_iter()
				.collect(),
			global: conf.global
		})
	}).map_or_else(|e| Err(e), |x| x)
}


fn default_checktime() -> u64 {
	15
}

fn default_port() -> u16 {
	9666
}

fn default_dns_refresh_time() -> u64 {
	120
}

fn default_addr() -> IpAddr {
	IpAddr::from(Ipv4Addr::new(0, 0, 0, 0))
}

fn default_enabled() -> AtomicBool {
	AtomicBool::new(true)
}

fn resolve_host(host: &str) -> Option<IpAddr> {
	host.parse().map_or_else(|_| {
		lookup_host(host).map(|x| x.get(0).map(|x| *x)).unwrap_or(None)
	}, |x| Some(x))
}

trait UrlQueryMaker {
	type R;
	type E;
	fn internal_query(&self, ip: &Option<IpAddr>) -> Result<Self::R, Self::E>;
	fn internal_resolve(&self) -> Option<IpAddr>;
}

trait Url {
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
struct HttpStruct {
	query: String,
	tls_enabled: bool,
	port: u16,
	host: String,
	path: String
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

struct UrlVisitor;

impl<'de> de::Visitor<'de> for UrlVisitor {
	type Value = Box<dyn Url + Send + Sync>;

	fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
		formatter.write_str("an url matching scheme://host:port/path")
	}

	fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
	where
		E: de::Error,
	{
		let scheme_and_remainder: Vec<&str> = value.splitn(2, "://").collect();
		if scheme_and_remainder.len() != 2 {
			return Err(de::Error::invalid_value(serde::de::Unexpected::Str(value), &self));
		}
		let host_and_path: Vec<&str> = scheme_and_remainder[1].splitn(2, '/').collect();
		let path = host_and_path
			.get(1)
			.and_then(|&x| Some(x.into()))
			.unwrap_or(String::new());

		let host_and_port: Vec<&str> = host_and_path[0].splitn(2, ':').collect();
		match scheme_and_remainder[0] {
			"http" | "https" =>
				Ok(Box::new(HttpStruct {
					query: value.into(),
					tls_enabled: scheme_and_remainder[0] == "https",
					port: host_and_port
							.get(1)
							.and_then(|x| str::parse::<u16>(x).ok())
							.unwrap_or_else(|| if scheme_and_remainder[0] == "https" { 443 } else { 80 })
					,
					host: host_and_port[0].into(),
					path
				})),
			_ => Err(de::Error::invalid_value(serde::de::Unexpected::Str(value), &self))
		}
	}
}

impl<'de> Deserialize<'de> for Box<dyn Url + Send + Sync> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		deserializer.deserialize_str(UrlVisitor)
	}
}


#[derive(Deserialize)]
struct Website {
	// TODO: sanitize this field
	name: String,
	url: Box<dyn Url + Send + Sync>,
	#[serde(default="default_checktime")]
	check_time_seconds: u64,
	#[serde(skip_deserializing, default = "default_enabled")]
	enabled: AtomicBool
}


impl std::hash::Hash for Website {
	fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
		self.name.hash(hasher);
	}
}

// We cannot simply derive that since AtomicBool is not comparable
impl PartialEq for Website {
	fn eq(&self, other: &Self) -> bool {
		self.name == other.name
	}
}
impl Eq for Website {}

#[derive(Deserialize, PartialEq)]
struct GlobalConfig {
	#[serde(default="default_port")]
	listen_port: u16,
	#[serde(default="default_addr")]
	listen_addr: IpAddr,
	#[serde(default="default_dns_refresh_time")]
	dns_refresh_time_seconds: u64
}

impl Default for GlobalConfig {
	fn default() -> Self {
		GlobalConfig {
			listen_port: default_port(),
			listen_addr: default_addr(),
			dns_refresh_time_seconds: default_dns_refresh_time()
		}
	}
}


#[derive(Deserialize)]
struct SerializedConfig {
	websites: Vec<Arc<Website>>,
	#[serde(flatten)]
	global: Arc<GlobalConfig>
}

#[derive(PartialEq)]
struct Config {
	websites: HashSet<Arc<Website>>,
	global: Arc<GlobalConfig>
}

impl Default for Config {
	fn default() -> Self {
		Config {
			websites: HashSet::new(),
			global: Arc::new(GlobalConfig::default())
		}
	}
}


#[derive(Debug, PartialEq)]
struct MetricData {
	status_code: Option<u16>,
	response_time: Option<Duration>,
	response_size: Option<u64>
}

impl From<attohttpc::Response> for MetricData {
	fn from(handle: attohttpc::Response) -> MetricData {
		MetricData {
			status_code: Some(handle.status().as_u16()),
			response_time: None,
			response_size: Some(handle.bytes().unwrap().len() as u64)
		}
	}
}

#[derive(Debug, PartialEq)]
enum MetricResult {
	Success(MetricData),
	Timeout,
	Error
}

impl MetricResult {
	fn map_success<F>(mut self, fun: F) -> MetricResult
	where F: Fn(&mut MetricData) {
		if let MetricResult::Success(ref mut md) = self {
			fun(md);
		}
		self
	}
}

impl From<Result<attohttpc::Response, attohttpc::Error>> for MetricResult  {
	fn from(res: Result<attohttpc::Response, attohttpc::Error>) -> MetricResult {
		match res {
			Ok(res) => MetricResult::Success(MetricData::from(res)),
			Err(e) => {
				if let attohttpc::ErrorKind::Io(e) = e.kind() {
					if e.kind() == std::io::ErrorKind::TimedOut {
						MetricResult::Timeout
					} else {
						MetricResult::Error
					}
				} else {
					MetricResult::Error
				}
			}
		}
	}
}


#[derive(Debug, PartialEq)]
enum WebsiteMessageType {
	MetricResult(MetricResult),
	Exit
}

#[derive(PartialEq)]
struct WatcherMessage {
	website: Arc<Website>,
	msg: WebsiteMessageType
}

#[derive(Debug, PartialEq)]
enum WebServMessage {
	ReloadConfig
}

fn loop_website(global_config: Arc<GlobalConfig>, ws: Arc<Website>, send_queue: Sender<WatcherMessage>) {
	let wait_time = Duration::new(ws.check_time_seconds, 0);
	let mut dns_resolution_start = Instant::now();
	// we cache the dns result so as to not spam our DNS resolver
	let mut ip = ws.url.resolve();
	loop {
		let start = Instant::now();
		if !ws.enabled.load(Ordering::Acquire) {
			send_queue.send(WatcherMessage {
				website: ws,
				msg: WebsiteMessageType::Exit
			}).unwrap();
			return;
		}
		if dns_resolution_start.elapsed() > Duration::new(global_config.dns_refresh_time_seconds, 0) {
			ip = ws.url.resolve();
			// let's reset the dns counter
			dns_resolution_start = Instant::now();
		}

		let ws_wrapper = AssertUnwindSafe(&ws.url);
		let res = match panic::catch_unwind(move || ws_wrapper.query(&ip) ) {
			Ok(x) => x,
			Err(_) => MetricResult::Error
		};

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

fn spawn_watcher(global_config: Arc<GlobalConfig>, website: &Arc<Website>, tx: Sender<WatcherMessage>) -> std::io::Result<()> {
	println!("Watching {}", website.name);
	let website = website.clone();
	thread::Builder::new()
		.name(format!("Q_{}", website.name))
		.spawn(move || loop_website(global_config.clone(), website, tx))?;
	Ok(())
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
			spawn_watcher(new_config.global.clone(), &w, tx_watchers.clone())?;
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
				spawn_watcher(new_config.global.clone(), x, tx_watchers.clone())?;
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

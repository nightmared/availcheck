use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::thread;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Instant, Duration};
use std::collections::HashMap;
use std::net::{SocketAddr, IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashSet;

use tiny_http::{Server, Method};

use curl::easy::Easy;

use serde::Deserialize;


fn get_base_dir() -> PathBuf {
	let config_base_dir = env::var_os("XDG_CONFIG_HOME")
		.unwrap_or(env::var_os("HOME")
			.unwrap_or(env::current_dir()
				.expect("Couldn't get the current directory")
					.into_os_string()));
	Path::new(&config_base_dir).join("availcheck/")
}

pub fn load_app_data<T: for <'a> Deserialize<'a>>(file_name: &str) -> std::io::Result<T> {
	let mut config_file_path = get_base_dir();
	config_file_path.push(file_name);
	// if your filename is not a valid utf-8 name, it's YOUR problem (like using a weird path and/or a
	// weird OS)
	load_yaml_file(&config_file_path.to_string_lossy())
}

pub fn load_yaml_file<T: for <'a> Deserialize<'a>>(file_name: &str) -> std::io::Result<T> {
	let mut fs = File::open(file_name).map_err(|e| {
		eprintln!("Unable to open the file '{}'.", file_name);
		e
	})?;
	let mut conf_buf = Vec::new();
	fs.read_to_end(&mut conf_buf)?;
	serde_yaml::from_slice::<T>(&conf_buf).map_err(|e| {
		eprintln!("Unable to parse the file '{}': {:?}", file_name, e);
		std::io::Error::from(std::io::ErrorKind::InvalidData)
	})
}

fn default_checktime() -> u64 {
    15
}

fn default_port() -> u16 {
    9666
}

fn default_addr() -> IpAddr {
    IpAddr::from(Ipv4Addr::new(0, 0, 0, 0))
}

fn default_enabled() -> AtomicBool {
    AtomicBool::new(true)
}

#[derive(Deserialize, Debug)]
struct Website {
    // TODO: sanitize this field
    name: String,
    url: String,
    #[serde(default="default_checktime")]
    check_time_seconds: u64,
    #[serde(skip_deserializing, default = "default_enabled")]
    enabled: AtomicBool
}


impl std::hash::Hash for Website {
    fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        self.url.hash(hasher);
    }
}

// We cannot simply derive that since AtomicBool is not comparable
impl PartialEq for Website {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}
impl Eq for Website {}

#[derive(Deserialize)]
struct Config {
    websites: HashSet<Arc<Website>>,
    #[serde(default="default_port")]
    listen_port: u16,
    #[serde(default="default_addr")]
    listen_addr: IpAddr
}


#[derive(Debug, PartialEq)]
struct MetricData {
    status_code: u32,
    response_time: Duration,
    response_size: u64
}

impl MetricData {
    fn new(handle: &mut Easy) -> MetricData {
        MetricData {
            status_code: handle.response_code().unwrap(),
            response_time: handle.total_time().unwrap()-handle.namelookup_time().unwrap(),
            response_size: handle.download_size().unwrap() as u64
        }
    }
}


#[derive(Debug, PartialEq)]
enum MetricResult {
    Success(MetricData),
    Timeout,
    Error
}

#[derive(Debug, PartialEq)]
enum WebsiteMessageType {
    MetricResult(MetricResult),
    Exit
}

#[derive(Debug, PartialEq)]
struct WebsiteMessage {
    website: Arc<Website>,
    msg: WebsiteMessageType
}

#[derive(Debug, PartialEq)]
enum Message {
    WebsiteMessage(WebsiteMessage),
    ReloadConfig
}

fn loop_website(ws: Arc<Website>, send_queue: Sender<Message>) {
    let wait_time = Duration::new(ws.check_time_seconds, 0);
    let mut start;
    let mut handle = Easy::new();
    loop {
        start = Instant::now();
        if !ws.enabled.load(Ordering::Acquire) {
            send_queue.send(Message::WebsiteMessage(WebsiteMessage {
                website: ws,
                msg: WebsiteMessageType::Exit
            })).unwrap();
            return;
        }

        handle.url(&ws.url).unwrap();
        handle.timeout(Duration::new(7, 0)).unwrap();
        let res = handle.perform();

        let msg = WebsiteMessage {
            website: ws.clone(),
            msg:
                WebsiteMessageType::MetricResult(
                    match res {
                        Ok(_) => MetricResult::Success(MetricData::new(&mut handle)),
                        Err(e) => {
                            if e.is_operation_timedout() {
                                MetricResult::Timeout
                            } else {
                                println!("Error on the website {}: {:?}", ws.name, e);
                                MetricResult::Error
                            }
                        }
                    }
                )
        };
        send_queue.send(Message::WebsiteMessage(msg)).unwrap();
        // prevent against suprious wakeups
        while start.elapsed() < wait_time {
            thread::sleep(wait_time-start.elapsed());
        }
    }
}

struct ServerState {
    internal: HashMap<Arc<Website>, MetricResult>
}

impl ServerState {
    fn add_metrics_to_string<T: std::fmt::Display>(&self, s: &mut String, name: &str, f: &dyn Fn(&MetricResult) -> Option<T>) {
        for ws in self.internal.keys() {
            match self.internal.get(ws) {
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
        self.internal.remove(website);
    }

    fn update_metrics(&mut self, website: &Arc<Website>, value: MetricResult) {
        if self.internal.contains_key(website) {
            *self.internal.get_mut(website).unwrap() = value;
        } else {
            self.internal.insert(website.clone(), value);
         }
    }
}

// TODO: add prometheus help to explain the meaning of the variables
fn gen_metrics_from_state(state: &ServerState) -> String {
    // simple heuristic to reduce pointless allocations
    let mut res = String::with_capacity(state.internal.keys().len() * 75);

    state.add_metrics_to_string(&mut res, "errors",
        &|msg| if let MetricResult::Error = msg { Some(1) } else { Some(0) });
    state.add_metrics_to_string(&mut res, "timeouts",
        &|msg| if let MetricResult::Timeout = msg { Some(1) } else { Some(0) });
    state.add_metrics_to_string(&mut res, "status_code",
        &|msg| if let MetricResult::Success(ref data) = msg { Some(data.status_code ) } else { None });
    state.add_metrics_to_string(&mut res, "response_time_ms",
        &|msg| if let MetricResult::Success(ref data) = msg { Some(data.response_time.as_millis() ) } else { None });
    state.add_metrics_to_string(&mut res, "response_size",
        &|msg| if let MetricResult::Success(ref data) = msg { Some(data.response_size ) } else { None });

    res
}

fn web_handler(server: Arc<Server>, state: Arc<RwLock<ServerState>>, tx: Sender<Message>) {
    loop {
        if let Ok(request) = server.recv() {
            if request.url() == "/metrics" {
                let data = gen_metrics_from_state(&state.read().unwrap());
                let res = tiny_http::Response::from_string(data);
                let _ = request.respond(res);
            } else if request.url() == "/server-reload" && *request.method() == Method::Post {
                // It is crucial to require the client to perform POST request to update the state
                // to prevent caching and to respect the fact that GET requests MUST be idempotent.
                //
                // Note: You MUST protect this endpoint, either by exposing the server only to
                // local adresses, or by adding a rule to your config if you have a
                // web server/load balancer in front on top of this server,
                // otherwise you risk a DoS if someone call this (very) frequently !
                tx.send(Message::ReloadConfig).unwrap();
                let res = tiny_http::Response::from_string("Server reloading requested, check the server logs");
                let _ = request.respond(res);
            } else {
                let res = tiny_http::Response::from_string("Get metrics at /metrics");
                let _ = request.respond(res);
            }
        }
    }
}

fn web_server(listen_addr: IpAddr, listen_port: u16, state: Arc<RwLock<ServerState>>, tx: Sender<Message>) {
    let server_config = tiny_http::ServerConfig {
        addr: SocketAddr::from((listen_addr, listen_port)),
        ssl: None
    };
    let server = Arc::new(Server::new(server_config).unwrap());

    // spawn 4 worker threads
    for i in 0..4 {
        let server = server.clone();
        let state = state.clone();
        let tx = tx.clone();
        thread::Builder::new()
            .name(format!("HTTP Serv {}", i))
            .spawn(move || web_handler(server, state, tx))
            .unwrap();
    }
}

fn spawn_watcher(website: &Arc<Website>, tx: Sender<Message>) -> std::io::Result<()> {
    println!("Watching {} under name {}", website.url, website.name);
    let website = website.clone();
    thread::Builder::new()
        .name(format!("Curl {}", website.name))
        .spawn(move || loop_website(website, tx))?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let mut config: Config =  load_app_data("config.yml")?;

    let (tx, rx) = channel();

    for website in &config.websites {
        spawn_watcher(website, tx.clone())?;
    }

    let state = Arc::new(RwLock::new(
            ServerState { internal: HashMap::new() }));

    let state_clone = state.clone();
    let tx_clone = tx.clone();
    let (listen_addr, listen_port) = (config.listen_addr, config.listen_port);
    // we spawn this thread for the only reason that it allows us to give HTTP
    // listener threads a meaningful name
    thread::Builder::new()
        .name(format!("HTTP Pool"))
        .spawn(move ||  web_server(listen_addr, listen_port, state_clone, tx_clone))?
        .join().expect("Couldn't spawn the HTTP Server");

    loop {
        match rx.recv() {
            Ok(Message::WebsiteMessage(msg)) => {
                let mut state_wrt = state.write().unwrap();
                // This website was deleted at runtime, let's remove it
                match msg.msg {
                    WebsiteMessageType::Exit  => state_wrt.delete_website(&msg.website),
                    WebsiteMessageType::MetricResult(e) => state_wrt.update_metrics(&msg.website, e)
                }
            },
            Ok(Message::ReloadConfig) => {
                // Updating the list of websites to check (it should be noted that changing the
                // http listening port or adress is not possible at runtime).
                println!("Server reloading asked, let's see what we can do for you...");
                // reload the config
                let new_config: Config = match load_app_data("config.yml") {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("Looks like your config file is invalid, aborting the procedure: {}", e);
                        continue;
                    }
                };


                // enumerate differences and apply them
                let differences = new_config.websites.symmetric_difference(&config.websites);
                let mut changes = 0;
                for x in differences {
                    changes += 1;
                    if !config.websites.contains(x) {
                        // website x has been added
                        spawn_watcher(x, tx.clone())?;
                    } else {
                        // website x has been deleted
                        x.enabled.store(false, Ordering::Release);
                    }
                }

                if new_config.listen_port != config.listen_port
                    || new_config.listen_addr != config.listen_addr {

                }
                config = new_config;
                println!("Server reloading finished successfully with {} changes !", changes);
            },
            Err(e) => {
                eprintln!("{} error encountered while reading on the main channel, exiting...", e);
                panic!();
            }
        }
    }
}

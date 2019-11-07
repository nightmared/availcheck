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

use tiny_http::Server;

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

// We cannot simply derive that since AtomicBool is not comparable
impl PartialEq for Website {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}
impl Eq for Website {}

#[derive(Deserialize)]
struct Config {
    websites: Vec<Arc<Website>>,
    #[serde(default="default_port")]
    listen_port: u16,
    #[serde(default="default_addr")]
    listen_addr: IpAddr
}

#[derive(Debug)]
struct AnswerData {
    status_code: u32,
    response_time: Duration,
    response_size: u64
}

impl AnswerData {
    fn new(handle: &mut Easy) -> AnswerData {
        AnswerData {
            status_code: handle.response_code().unwrap(),
            response_time: handle.total_time().unwrap()-handle.namelookup_time().unwrap(),
            response_size: handle.download_size().unwrap() as u64
        }
    }
}

#[derive(Debug)]
enum MessageType {
    Answer(AnswerData),
    Timeout,
    Error,
    Exit
}

#[derive(Debug)]
struct Message {
    website: Arc<Website>,
    msg: MessageType
}

fn loop_website(ws: Arc<Website>, send_queue: Sender<Message>) {
    let wait_time = Duration::new(ws.check_time_seconds, 0);
    let mut start;
    loop {
        if !ws.enabled.load(Ordering::Acquire) {
            send_queue.send(Message {
                website: ws,
                msg: MessageType::Exit
            }).unwrap();
            return;
        }

        start = Instant::now();
        let mut handle = Easy::new();
        // TODO: considering we are going to do plenty of dns lookups, caching should be used
        handle.url(&ws.url).unwrap();
        handle.timeout(Duration::new(7, 0)).unwrap();
        let res = handle.perform();

        let msg = Message {
            website: ws.clone(),
            msg:
                match res {
                    Ok(_) => MessageType::Answer(AnswerData::new(&mut handle)),
                    Err(e) => {
                        if e.is_operation_timedout() {
                            MessageType::Timeout
                        } else {
                            println!("Error on the website {}: {:?}", ws.name, e);
                            MessageType::Error
                        }
                    }
                }
        };
        send_queue.send(msg).unwrap();
        // prevent against suprious wakeups
        while start.elapsed() < wait_time {
            thread::sleep(wait_time-start.elapsed());
        }
    }
}

// The invariant the ServerState struct seeks to maintain is that at any (observable)
// time, there exists a one-to-one mapping between the elements of the 'websites'
// field and those of the the 'last_state' field
struct ServerState {
    websites: Vec<Arc<Website>>,
    // indexed by the website url
    last_state: HashMap<String, MessageType>
}

impl ServerState {
    fn add_metrics_to_string<T: std::fmt::Display>(&self, s: &mut String, name: &str, f: &dyn Fn(&MessageType) -> Option<T>) {
        for ws in &self.websites {
            // this unwrap is "safe" as per our invariant
            if let Some(v) = f(self.last_state.get(&ws.url).unwrap()) {
                s.push_str(format!("availcheck_{}{{website=\"{}\"}} {}\n", name, ws.name, v).as_str());
            }
        }
    }
}

// TODO: add prometheus help to explain the meaning of the variables
fn gen_metrics_from_state(state: &ServerState) -> String {
    // simple heuristic to reduce pointless allocations
    let mut res = String::with_capacity(state.websites.len() * 75);

    state.add_metrics_to_string(&mut res, "errors",
        &|msg| if let &MessageType::Error = msg { Some(1) } else { None });
    state.add_metrics_to_string(&mut res, "timeouts",
        &|msg| if let &MessageType::Timeout = msg { Some(1) } else { None });
    state.add_metrics_to_string(&mut res, "status_code",
        &|msg| if let &MessageType::Answer(ref data) = msg { Some(data.status_code ) } else { None });
    state.add_metrics_to_string(&mut res, "response_time_ms",
        &|msg| if let &MessageType::Answer(ref data) = msg { Some(data.response_time.as_millis() ) } else { None });
    state.add_metrics_to_string(&mut res, "response_size",
        &|msg| if let &MessageType::Answer(ref data) = msg { Some(data.response_size ) } else { None });

    res
}

fn web_server(config: Config, state: Arc<RwLock<ServerState>>) {
    let server_config = tiny_http::ServerConfig {
        addr: SocketAddr::from((config.listen_addr, config.listen_port)),
        ssl: None
    };
    let server = Arc::new(Server::new(server_config).unwrap());

    // spawn 4 worker threads
    for i in 0..4 {
        let server = server.clone();
        let state = state.clone();
        thread::Builder::new()
            .name(format!("HTTP Serv {}", i))
            .spawn(move || {
            loop {
                if let Ok(request) = server.recv() {
                    if request.url() == "/metrics" {
                        let data = gen_metrics_from_state(&state.read().unwrap());
                        let res = tiny_http::Response::from_string(data);
                        let _ = request.respond(res);
                    } else {
                        let res = tiny_http::Response::from_string("Get metrics at /metrics");
                        let _ = request.respond(res);
                    }
                }
            }
        }).unwrap();
    }
}

// TODO: support runtime upgrade

fn main() -> std::io::Result<()> {
    let config: Config =  load_app_data("config.yml")?;

    let (tx, rx) = channel();

    for website in &config.websites {
        println!("Watching {} under name {}", website.url, website.name);
        let website = website.clone();
        let tx = tx.clone();
        thread::Builder::new()
            .name(format!("Curl {}", website.name))
            .spawn(move || loop_website(website, tx))?;
    }

    let state = Arc::new(RwLock::new(
            ServerState { websites: vec![], last_state: HashMap::new() }));

    let state_clone = state.clone();
    // we spawn this thread for the only reason that it allows us to give HTTP
    // listener threads a meaningful name
    thread::Builder::new()
        .name(format!("HTTP Pool"))
        .spawn(move ||  web_server(config, state_clone))?;

    loop {
        match rx.recv() {
            Ok(msg) => {
                let mut state_wrt = state.write().unwrap();
                if state_wrt.websites.contains(&msg.website) {
                    *state_wrt.last_state.get_mut(&msg.website.url).unwrap() = msg.msg;
                } else {
                    // first time we see this website, let's add it to the list
                    state_wrt.websites.push(msg.website.clone());
                    state_wrt.last_state.insert(msg.website.url.clone(), msg.msg);

                }
            },
            Err(e) => {
                eprintln!("{:?} error encountered while reading on a channel, exiting...", e);
                panic!();
            }
        }
    }
}

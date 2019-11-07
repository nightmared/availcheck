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

#[derive(Deserialize, Debug, PartialEq)]
struct Website {
    // TODO: sanitize this field
    name: String,
    url: String,
    #[serde(default="default_checktime")]
    check_time_seconds: u64
}

#[derive(Deserialize)]
struct FileConfig {
    websites: Vec<Website>,
    #[serde(default="default_port")]
    listen_port: u16,
    #[serde(default="default_addr")]
    listen_addr: IpAddr
}

struct Config {
    websites: Vec<Arc<Website>>,
    listen_port: u16,
    listen_addr: IpAddr
}

impl From<FileConfig> for Config {
    fn from(fc: FileConfig) -> Config {
        Config {
            websites:
                fc.websites
                    .into_iter()
                    .map(|x| Arc::new(x))
                    .collect(),
            listen_addr: fc.listen_addr,
            listen_port: fc.listen_port
        }
    }
}

#[derive(Debug)]
struct AnswerData {
    status_code: u32,
    response_time: Duration,
    response_size: u64
}

impl AnswerData {
    fn new(handle: &mut Easy, data_length: u64) -> AnswerData {
        AnswerData {
            status_code: handle.response_code().unwrap(),
            response_time: handle.total_time().unwrap()-handle.namelookup_time().unwrap(),
            response_size: data_length
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
        start = Instant::now();
        let mut handle = Easy::new();
        // TODO: considering we are going to do plenty of dns lookups, caching should be used
        handle.url(&ws.url).unwrap();
        handle.timeout(Duration::new(7, 0)).unwrap();

        let mut res_size = 0;
        let res = {
            let mut transfer = handle.transfer();
            transfer.write_function(|new_data| {
                res_size += new_data.len();
                Ok(res_size)
            }).unwrap();
            transfer.perform()
        };
        let msg = Message {
            website: ws.clone(),
            msg:
                match res {
                    Ok(_) => MessageType::Answer(AnswerData::new(&mut handle, res_size as u64)),
                    Err(e) => {
                        if e.is_operation_timedout() {
                            MessageType::Timeout
                        } else {
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

// The invariant the ServerState seeks to maintain is that at any (observable) time,
// there exists a one-to-one mapping between the elements of the 'websites'
// field and those of the the 'last_state' field
struct ServerState {
    websites: Vec<Arc<Website>>,
    // indexed by the website url
    last_state: HashMap<String, MessageType>
}
fn add_metrics_to_string(datas: Vec<(&Arc<Website>, impl std::fmt::Display)>, name: &str, s: &mut String) {
    for val in datas {
        s.push_str(format!("availcheck_{}{{website=\"{}\"}} {}\n", name, val.0.name, val.1).as_str());
    }
}

fn gen_metrics_from_state(state: &ServerState) -> String {
    // simple heuristic to reduce pointless allocations
    let mut errors = Vec::with_capacity(state.websites.len());
    let mut timeouts = Vec::with_capacity(state.websites.len());
    let mut status_code = Vec::with_capacity(state.websites.len());
    let mut response_time_ms = Vec::with_capacity(state.websites.len());
    let mut response_size = Vec::with_capacity(state.websites.len());
    for val in &state.websites {
        // this unwrap is "safe" as per our invariant
        match state.last_state.get(&val.url).unwrap() {
            MessageType::Answer(data) => {
                errors.push((val, 0));
                timeouts.push((val, 0));
                status_code.push((val, data.status_code));
                response_time_ms.push((val, data.response_time.as_millis()));
                response_size.push((val, data.response_size));
            },
            MessageType::Timeout => timeouts.push((val, 1)),
            MessageType::Error => errors.push((val, 1)),
            MessageType::Exit => ()
        }
    }
    let mut res = String::with_capacity(state.websites.len() * 75);
    add_metrics_to_string(errors, "errors", &mut res);
    add_metrics_to_string(timeouts, "timeouts", &mut res);
    add_metrics_to_string(status_code, "status_code", &mut res);
    add_metrics_to_string(response_time_ms, "response_time_ms", &mut res);
    add_metrics_to_string(response_size, "response_size", &mut res);
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
    let config = {
        let file_config: FileConfig = load_app_data("config.yml")?;
        Config::from(file_config)
    };

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
                // println!("{:?}", msg);
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

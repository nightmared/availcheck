use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::thread;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Instant, Duration};

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

#[derive(Deserialize, Debug)]
struct Website {
    url: String,
    #[serde(default)]
    check_time_seconds: CheckTime
}

#[derive(Deserialize)]
struct FileConfig {
    websites: Vec<Website>
}

struct Config {
    websites: Vec<Arc<Website>>
}

impl From<FileConfig> for Config {
    fn from(fc: FileConfig) -> Config {
        Config {
            websites:
                fc.websites
                    .into_iter()
                    .map(|x| Arc::new(x))
                    .collect()
        }

    }
}

#[derive(Deserialize, Debug)]
struct CheckTime(u64);

// Check every 15 seconds by default
impl std::default::Default for CheckTime {
    fn default() -> CheckTime {
        CheckTime(15)
    }
}


#[derive(Debug)]
enum MessageType {
    Response(u32, Duration),
    Timeout,
    Error
}

#[derive(Debug)]
struct Message {
    website: Arc<Website>,
    msg: MessageType
}

fn loop_website(ws: Arc<Website>, send_queue: Sender<Message>) {
    let wait_time = Duration::new(ws.check_time_seconds.0, 0);
    let mut start;
    loop {
        start = Instant::now();
        let mut handle = Easy::new();
        // TODO: considering we are going to do plenty of dns lookups, caching should be used
        handle.url(&ws.url).unwrap();
        handle.timeout(Duration::new(3, 0)).unwrap();

        send_queue.send(Message {
                website: ws.clone(),
                msg: 
                    match handle.perform() {
                        Ok(_) => MessageType::Response(handle.response_code().unwrap(), handle.total_time().unwrap()-handle.namelookup_time().unwrap()),
                        Err(e) => if e.is_operation_timedout() {
                                MessageType::Timeout
                            } else {
                                MessageType::Error
                            }
                    }
        }).unwrap();
        // prevent against suprious wakeups
        while start.elapsed() < wait_time {
            thread::sleep(wait_time-start.elapsed());
        }
    }
}


fn main() -> std::io::Result<()> {
    let config = {
        let file_config: FileConfig = load_app_data("config.yml")?;
        Config::from(file_config)
    };

    let (tx, rx) = channel();

    for website in config.websites {
        println!("{:?}", website);
        let website = website.clone();
        let tx = tx.clone();
        thread::spawn(|| loop_website(website, tx));
    }

    loop {
        match rx.recv() {
            Ok(msg) => {
                println!("{:?}", msg);

            },
            Err(e) => {
                eprintln!("{:?} error encountered while reading on a chennel, exiting...", e);
                panic!();
            }
        }
    }
}

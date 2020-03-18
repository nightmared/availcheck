use std::io::{self, prelude::*};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::collections::HashSet;
use std::sync::Arc;
use std::net::{IpAddr, Ipv4Addr};
use std::fs::File;
use std::env;

use serde::{Deserialize, Deserializer};
use serde::de;

use crate::query_providers::Url;

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


pub fn load_config() -> std::io::Result<Config> {
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
			/*"http" | "https" =>
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
				})),*/
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
pub struct Website {
	// TODO: sanitize this field
	pub name: String,
	pub url: Box<dyn Url + Send + Sync>,
	#[serde(default="default_checktime")]
	pub check_time_seconds: u64,
	#[serde(skip_deserializing, default = "default_enabled")]
	pub enabled: AtomicBool
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
pub struct GlobalConfig {
	#[serde(default="default_port")]
	pub listen_port: u16,
	#[serde(default="default_addr")]
	pub listen_addr: IpAddr,
	#[serde(default="default_dns_refresh_time")]
	pub dns_refresh_time_seconds: u64
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
pub struct Config {
	pub websites: HashSet<Arc<Website>>,
	pub global: Arc<GlobalConfig>
}

impl Default for Config {
	fn default() -> Self {
		Config {
			websites: HashSet::new(),
			global: Arc::new(GlobalConfig::default())
		}
	}
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

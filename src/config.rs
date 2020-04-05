use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::collections::HashSet;
use std::sync::Arc;
use std::net::{IpAddr, Ipv4Addr};
use std::env;
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use serde::de;
use tokio::fs;

use crate::query_providers::{Url, HttpStruct, HttpWrapper};
use crate::dns::{DnsResolverServer, DnsResolverServerState};

fn get_base_dir() -> PathBuf {
	let config_base_dir = env::var_os("XDG_CONFIG_HOME")
		.unwrap_or(env::var_os("HOME")
			.unwrap_or(env::current_dir()
				.expect("Couldn't get the current directory")
					.into_os_string()));
	Path::new(&config_base_dir).join("availcheck/")
}

async fn load_yaml_file<T: for <'a> Deserialize<'a>>(file_name: &str) -> std::io::Result<T> {
	let file_content = fs::read_to_string(file_name).await?;
	serde_yaml::from_str::<T>(&file_content).map_err(|e| {
		eprintln!("Unable to parse the file '{}': {:?}", file_name, e);
		io::Error::from(io::ErrorKind::InvalidData)
	})
}

async fn load_app_data<T: for <'a> Deserialize<'a>>(file_name: &str) -> std::io::Result<T> {
	let mut config_file_path = get_base_dir();
	config_file_path.push(file_name);
	// if your filename is not a valid utf-8 name, it's YOUR problem (like using a weird path
	// and/or doing weird things).
	// TODO: look into OsStr
	load_yaml_file(&config_file_path.to_string_lossy()).await
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


pub async fn load_config() -> std::io::Result<Config> {
	let conf: SerializedConfig = load_app_data("config.yml").await?;

	let mut dns_resolver = DnsResolverServer::new(
		DnsResolverServerState::new(
			Duration::new(conf.global.dns_refresh_time_seconds, 0)
		)
	);

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

	let new_config = Config {
		websites: conf.websites
			.into_iter()
			.map(|mut x| {
				let (tx, rx) = dns_resolver.gen_client();
				x.url.gen_client(tx, rx);
				Arc::new(x)
			}).collect(),
		global: conf.global
	};

	tokio::spawn(dns_resolver.run());

	Ok(new_config)
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

		match scheme_and_remainder[0] {
			"http" | "https" =>
				Ok(Box::new(HttpWrapper::new(HttpStruct {
					query: value.into()
				}))),
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
	websites: Vec<Website>,
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

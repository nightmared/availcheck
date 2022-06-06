use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::Arc;

use serde::de;
use serde::{Deserialize, Deserializer};
use tokio::fs;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::query_providers::{HttpStruct, Url};

async fn load_yaml_file<T: for<'a> Deserialize<'a>>(mut fd: fs::File) -> std::io::Result<T> {
    let mut file_content = String::new();
    fd.read_to_string(&mut file_content).await?;
    serde_yaml::from_str::<T>(&file_content).map_err(|e| {
        eprintln!("Unable to parse the file '{:?}': {:?}", fd, e);
        io::Error::from(io::ErrorKind::InvalidData)
    })
}

// This function only works if you give it a sorted list
fn get_duplicates<'a>(sorted_vec: &'a Vec<&String>) -> HashSet<&'a str> {
    let mut duplicates = HashSet::new();
    for i in 1..sorted_vec.len() {
        if sorted_vec[i - 1] == sorted_vec[i] {
            duplicates.insert(sorted_vec[i].as_ref());
        }
    }
    duplicates
}

pub async fn load_config(config_path: &Path) -> std::io::Result<Config> {
    let conf: SerializedConfig = load_yaml_file(File::open(config_path).await?).await?;

    let mut names = conf
        .websites
        .iter()
        .map(|x| &x.name)
        .collect::<Vec<&String>>();
    names.sort();
    let duplicates = get_duplicates(&names);
    if duplicates.len() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Duplicate names in your config file: {:?}", duplicates),
        ));
    }

    let new_config = Config {
        websites: conf.websites.into_iter().map(Arc::new).collect(),
        global: conf.global,
    };

    Ok(new_config)
}

impl<'de> Deserialize<'de> for Box<dyn Url + Send + Sync> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let url = url::Url::deserialize(deserializer)?;
        match url.scheme() {
            "http" | "https" => Ok(Box::new(HttpStruct { query_url: url })),
            _ => Err(de::Error::custom("Unsupported URL scheme")),
        }
    }
}

#[derive(Deserialize)]
pub struct Website {
    // TODO: sanitize this field
    pub name: String,
    pub url: Box<dyn Url + Send + Sync>,
    #[serde(default = "default_checktime")]
    pub check_time_seconds: u64,
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
    #[serde(default = "default_port")]
    pub listen_port: u16,
    #[serde(default = "default_addr")]
    pub listen_addr: IpAddr,
    #[serde(default = "default_tracing_enabled")]
    pub enable_tracing: bool,
    #[serde(default = "default_dns_refresh_time")]
    pub dns_refresh_time_seconds: u64,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        GlobalConfig {
            listen_port: default_port(),
            listen_addr: default_addr(),
            enable_tracing: default_tracing_enabled(),
            dns_refresh_time_seconds: default_dns_refresh_time(),
        }
    }
}

#[derive(Deserialize)]
struct SerializedConfig {
    websites: Vec<Website>,
    #[serde(flatten)]
    global: Arc<GlobalConfig>,
}

#[derive(PartialEq)]
pub struct Config {
    pub websites: HashSet<Arc<Website>>,
    pub global: Arc<GlobalConfig>,
}

fn default_checktime() -> u64 {
    15
}

fn default_port() -> u16 {
    9666
}

fn default_tracing_enabled() -> bool {
    false
}

fn default_dns_refresh_time() -> u64 {
    120
}

fn default_addr() -> IpAddr {
    IpAddr::from(Ipv4Addr::new(0, 0, 0, 0))
}

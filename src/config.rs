use std::collections::HashSet;
use std::env;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::de;
use serde::{Deserialize, Deserializer};
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::dns::ExternalDnsResolverPoller;
use crate::query_providers::{HttpStruct, Url};

fn get_base_dirs() -> Vec<PathBuf> {
    let mut res = Vec::new();
    if let Some(x) = env::var_os("XDG_CONFIG_HOME").map(|x| Path::new(&x).join("availcheck")) {
        res.push(x);
    }

    if let Some(x) = env::var_os("HOME").map(|x| Path::new(&x).join(".config").join("availcheck")) {
        res.push(x);
    }

    res.push(PathBuf::from(
        env::current_dir()
            .expect("Couldn't get the current directory")
            .into_os_string(),
    ));

    res
}

async fn load_yaml_file<T: for<'a> Deserialize<'a>>(mut fd: fs::File) -> std::io::Result<T> {
    let mut file_content = String::new();
    fd.read_to_string(&mut file_content).await?;
    serde_yaml::from_str::<T>(&file_content).map_err(|e| {
        eprintln!("Unable to parse the file '{:?}': {:?}", fd, e);
        io::Error::from(io::ErrorKind::InvalidData)
    })
}

async fn load_app_data<T: for<'a> Deserialize<'a>>(file_name: &str) -> std::io::Result<T> {
    let mut config_file_paths = get_base_dirs();
    for p in &mut config_file_paths {
        p.push(file_name);
        if let Ok(fd) = fs::File::open(p).await {
            return load_yaml_file(fd).await;
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Config file not found",
    ))
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

pub async fn load_config() -> std::io::Result<(Config, ExternalDnsResolverPoller)> {
    let conf: SerializedConfig = load_app_data("availcheck.yml").await?;

    let dns_resolver =
        ExternalDnsResolverPoller::new(Duration::new(conf.global.dns_refresh_time_seconds, 0));

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

    Ok((new_config, dns_resolver))
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
            return Err(de::Error::invalid_value(
                serde::de::Unexpected::Str(value),
                &self,
            ));
        }

        match scheme_and_remainder[0] {
            "http" | "https" => Ok(Box::new(HttpStruct {
                query: value.into(),
            })),
            _ => Err(de::Error::invalid_value(
                serde::de::Unexpected::Str(value),
                &self,
            )),
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

#[derive(Deserialize, Debug)]
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
    #[serde(default = "default_dns_refresh_time")]
    pub dns_refresh_time_seconds: u64,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        GlobalConfig {
            listen_port: default_port(),
            listen_addr: default_addr(),
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

fn default_dns_refresh_time() -> u64 {
    120
}

fn default_addr() -> IpAddr {
    IpAddr::from(Ipv4Addr::new(0, 0, 0, 0))
}

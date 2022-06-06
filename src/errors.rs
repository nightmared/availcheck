use std::net::IpAddr;

use http::uri::InvalidUri;
use hyper::client::connect::dns::Name;
use opentelemetry::trace::TraceError;
use thiserror::Error;
use tokio::{
    sync::{
        mpsc::error::SendError,
        oneshot::{error::RecvError, Sender},
    },
    task::JoinError,
    time::error::Elapsed,
};
use tracing_subscriber::util::TryInitError;
use trust_dns_resolver::error::ResolveError;

use crate::server::ServerReload;

#[derive(Error, Debug)]
pub enum DnsError {
    #[error("DNS resolution failed")]
    DnsResolution(#[from] ResolveError),
    #[error("No DNS entry for that domain")]
    NoDNSEntry(String),
    #[error("sending on a channel failed")]
    Send(Box<Result<IpAddr, DnsError>>),
    #[error("receiving a oneshot message failed")]
    Recv(#[from] RecvError),
    #[error("sending the DNS query failed")]
    SendQuery(#[from] SendError<(Name, Sender<Result<IpAddr, DnsError>>)>),
}

#[derive(Error, Debug)]
pub enum QueryError {
    #[error("DNS error")]
    Dns(#[from] DnsError),
    #[error("something timed out")]
    Timeout(#[from] Elapsed),
    #[error("Hyper error")]
    Hyper(#[from] hyper::Error),
    #[error("Invalid URI")]
    InvalidURI(#[from] InvalidUri),
    #[error("HTTP error")]
    HTTP(#[from] http::Error),
}

#[derive(Error, Debug)]
pub enum ServerError {
    #[error("IO error")]
    IO(#[from] std::io::Error),
    #[error("sending a reload message failed")]
    ReloadMessage(#[from] SendError<ServerReload>),
}

#[derive(Error, Debug)]
pub(crate) enum MainLoopError {
    #[error("IO error")]
    IO(#[from] std::io::Error),
    #[error("DNS error")]
    Dns(#[from] DnsError),
    #[error("query error")]
    Query(#[from] QueryError),
    #[error("server error")]
    Server(#[from] ServerError),
    #[error("tracing error")]
    Tracing(#[from] TraceError),
    #[error("tracing initialization failed")]
    TracingInit(#[from] TryInitError),
    #[error("sending an empty message failed")]
    SendingEmptyMessage(#[from] SendError<()>),
    #[error("polling an a tokio JoinHandle failed")]
    JoinError(#[from] JoinError),
}

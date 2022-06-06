use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use opentelemetry::sdk::{export::trace::stdout, trace::Config};
use tokio::sync::mpsc::channel;

use tracing_subscriber::prelude::*;

mod config;
mod dns;
mod errors;
mod metrics;
mod query_providers;
mod server;
mod task;

use config::{load_config, Website};
use errors::MainLoopError;
use metrics::MetricResult;
use task::select_vec;

/// A basic prometheus exporter for probing remote endpoints
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Path to the configuration file
    #[clap(short, long)]
    config_file: PathBuf,
}

// TODO: add logging

fn add_metrics_to_string<T: std::fmt::Display>(
    websites: &HashMap<Arc<Website>, MetricResult>,
    s: &mut String,
    name: &str,
    f: &dyn Fn(&MetricResult) -> Option<T>,
) {
    for ws in websites.keys() {
        match websites.get(ws) {
            None => eprintln!("Something is DEEEPLY wrong here !"),
            Some(e) => {
                if let Some(v) = f(&e) {
                    s.push_str(
                        format!("availcheck_{}{{website=\"{}\"}} {}\n", name, ws.name, v).as_str(),
                    );
                }
            }
        }
    }
}

// TODO: add prometheus help to explain the meaning of the variables
fn gen_metrics(websites: &HashMap<Arc<Website>, MetricResult>) -> String {
    // simple heuristic to reduce pointless allocations
    let mut res = String::with_capacity(websites.keys().len() * 75);

    add_metrics_to_string(websites, &mut res, "errors", &|msg| {
        if let MetricResult::Error = msg {
            Some(1)
        } else {
            Some(0)
        }
    });
    add_metrics_to_string(websites, &mut res, "timeouts", &|msg| {
        if let MetricResult::Timeout = msg {
            Some(1)
        } else {
            Some(0)
        }
    });
    add_metrics_to_string(websites, &mut res, "status_code", &|msg| {
        if let MetricResult::Success(ref data) = msg {
            data.status_code
        } else {
            None
        }
    });
    add_metrics_to_string(websites, &mut res, "response_time_ms", &|msg| {
        if let MetricResult::Success(ref data) = msg {
            data.response_time.map(|x| x.as_millis())
        } else {
            None
        }
    });
    add_metrics_to_string(websites, &mut res, "response_size", &|msg| {
        if let MetricResult::Success(ref data) = msg {
            data.response_size
        } else {
            None
        }
    });

    res
}

#[tokio::main]
async fn main() -> Result<(), MainLoopError> {
    let args = Args::parse();

    let config = load_config(args.config_file.as_path()).await?;

    if config.global.enable_tracing {
        let tracer = opentelemetry_jaeger::new_pipeline()
            .with_service_name("availcheck")
            .install_simple()?;
        let opentelemetry = tracing_opentelemetry::layer().with_tracer(tracer);

        tracing_subscriber::registry()
            .with(opentelemetry)
            .try_init()?;
    };

    let (mut app, mut webserv_fut, mut dns_resolver_fut, mut queriers, mut rx_webserv) =
        server::App::new(config).await?;

    let (tx_querier_reload, rx_querier_reload) = channel(1);
    // force loading the list of websites to monitor at startup
    tx_querier_reload.send(()).await?;

    let mut querier_fut = Box::pin(queriers.run(rx_querier_reload));

    let mut old_tasks = Vec::new();

    loop {
        tokio::select! {
            _ = &mut webserv_fut => {
                panic!("The webserver just crashed !");
            },
            _ = &mut querier_fut => {
                panic!("The querier service just crashed !");
            },
            _ = &mut dns_resolver_fut => {
                panic!("The dns service just crashed !");
            },
            Some(server::ServerReload {}) = rx_webserv.recv() => {
                if let Some((web_reload_fut, new_dns_resolver_fut)) =
                    app.load_new_config(args.config_file.as_path())
                        .await
                        .unwrap() {
                     // relaunch a new web server is necessary;
                    if let Some(new_webserv_fut) = web_reload_fut {
                        // poll the old web server to graceful completion
                        let _ = app.tx_shutdown_webserv.send(());
                        old_tasks.push(webserv_fut);
                        webserv_fut = new_webserv_fut;
                    }
                    old_tasks.push(dns_resolver_fut);
                    dns_resolver_fut = new_dns_resolver_fut;
                    tx_querier_reload.send(()).await?;
                }
            },
            Some(Err(e)) = select_vec(&mut old_tasks) => {
                println!("{:?}", e);
            },

        }
    }
}

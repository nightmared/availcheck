use std::time::Duration;
use std::sync::Arc;
use std::collections::HashMap;

use crate::config::Website;

#[derive(Debug, PartialEq)]
pub struct MetricData {
	pub status_code: Option<u16>,
	pub response_time: Option<Duration>,
	pub response_size: Option<u64>
}

#[derive(Debug, PartialEq)]
pub enum MetricResult {
	Success(MetricData),
	Timeout,
	Error
}

impl MetricResult {
	pub fn map_success<F>(mut self, fun: F) -> MetricResult
		where F: Fn(&mut MetricData) {

		if let MetricResult::Success(ref mut md) = self {
			fun(md);
		}
		self
	}
}

fn add_metrics_to_string<T: std::fmt::Display>(websites: &HashMap<Arc<Website>, MetricResult>, s: &mut String, name: &str, f: &dyn Fn(&MetricResult) -> Option<T>) {
	for ws in websites.keys() {
		match websites.get(ws) {
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

// TODO: add prometheus help to explain the meaning of the variables
pub fn gen_metrics(websites: &HashMap<Arc<Website>, MetricResult>) -> String {
	// simple heuristic to reduce pointless allocations
	let mut res = String::with_capacity(websites.keys().len() * 75);

	add_metrics_to_string(websites, &mut res, "errors",
		&|msg| if let MetricResult::Error = msg { Some(1) } else { Some(0) });
	add_metrics_to_string(websites, &mut res, "timeouts",
		&|msg| if let MetricResult::Timeout = msg { Some(1) } else { Some(0) });
	add_metrics_to_string(websites, &mut res, "status_code",
		&|msg| if let MetricResult::Success(ref data) = msg { data.status_code } else { None });
	add_metrics_to_string(websites, &mut res, "response_time_ms",
		&|msg| if let MetricResult::Success(ref data) = msg { data.response_time.map(|x| x.as_millis()) } else { None });
	add_metrics_to_string(websites, &mut res, "response_size",
		&|msg| if let MetricResult::Success(ref data) = msg { data.response_size } else { None });

	res
}

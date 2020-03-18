use std::time::Duration;
use std::sync::Arc;

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

#[derive(Debug, PartialEq)]
pub enum WebsiteMessageType {
	MetricResult(MetricResult),
	Exit
}

#[derive(PartialEq)]
pub struct WatcherMessage {
	pub website: Arc<Website>,
	pub msg: WebsiteMessageType
}

#[derive(Debug, PartialEq)]
pub enum WebServMessage {
	ReloadConfig
}

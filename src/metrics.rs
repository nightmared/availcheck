use std::time::Duration;
use std::sync::Arc;

use crate::config::Website;

#[derive(Debug, PartialEq)]
pub struct MetricData {
	pub status_code: Option<u16>,
	pub response_time: Option<Duration>,
	pub response_size: Option<u64>
}

impl From<attohttpc::Response> for MetricData {
	fn from(handle: attohttpc::Response) -> MetricData {
		MetricData {
			status_code: Some(handle.status().as_u16()),
			response_time: None,
			response_size: Some(handle.bytes().unwrap().len() as u64)
		}
	}
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

impl From<Result<attohttpc::Response, attohttpc::Error>> for MetricResult  {
	fn from(res: Result<attohttpc::Response, attohttpc::Error>) -> MetricResult {
		match res {
			Ok(res) => MetricResult::Success(MetricData::from(res)),
			Err(e) => {
				if let attohttpc::ErrorKind::Io(e) = e.kind() {
					if e.kind() == std::io::ErrorKind::TimedOut {
						MetricResult::Timeout
					} else {
						MetricResult::Error
					}
				} else {
					MetricResult::Error
				}
			}
		}
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

use std::time::Duration;

#[derive(Debug, PartialEq, Clone)]
pub struct MetricData {
    pub status_code: Option<u16>,
    pub response_time: Option<Duration>,
    pub response_size: Option<u64>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum MetricResult {
    Success(MetricData),
    Timeout,
    Error,
}

impl MetricResult {
    pub fn map_success<F>(mut self, fun: F) -> MetricResult
    where
        F: Fn(&mut MetricData),
    {
        if let MetricResult::Success(ref mut md) = self {
            fun(md);
        }
        self
    }
}

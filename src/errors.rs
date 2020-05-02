
#[derive(Debug)]
pub enum Error {
	Curl(curl::Error),
	Serde(serde_yaml::Error),
	Timeout(tokio::time::Elapsed),
	InvalidUri(http::uri::InvalidUri),
	InvalidRequest(http::Error),
	LooperCrashed,
	DnsResolutionFailed
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Error::Curl(e) => Some(e),
			Error::Serde(e) => Some(e),
			Error::Timeout(e) => Some(e),
			Error::InvalidUri(e) => Some(e),
			Error::InvalidRequest(e) => Some(e),
			Error::LooperCrashed => None,
			Error::DnsResolutionFailed => None
		}
    }
}

impl From<curl::Error> for Error {
	fn from(val: curl::Error) -> Self {
		Error::Curl(val)
	}
}

impl From<serde_yaml::Error> for Error {
	fn from(val: serde_yaml::Error) -> Self {
		Error::Serde(val)
	}
}

impl From<tokio::time::Elapsed> for Error {
	fn from(val: tokio::time::Elapsed) -> Self {
		Error::Timeout(val)
	}
}

impl From<http::uri::InvalidUri> for Error {
	fn from(val: http::uri::InvalidUri) -> Self {
		Error::InvalidUri(val)
	}
}

impl From<http::Error> for Error {
	fn from(val: http::Error) -> Self {
		Error::InvalidRequest(val)
	}
}

impl From<c_ares::Error> for Error {
	fn from(_val: c_ares::Error) -> Self {
		Error::DnsResolutionFailed
	}
}

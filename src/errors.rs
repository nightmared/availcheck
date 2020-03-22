
#[derive(Debug)]
pub enum Error {
	Hyper(hyper::Error),
	Serde(serde_yaml::Error),
	Timeout(tokio::time::Elapsed),
	InvalidUri(http::uri::InvalidUri),
	InvalidRequest(http::Error),
	ResolutionFailed,
	ChannelFailure(tokio::sync::mpsc::error::SendError<hyper::client::connect::dns::Name>)
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Error::Hyper(e) => Some(e),
			Error::Serde(e) => Some(e),
			Error::Timeout(e) => Some(e),
			Error::InvalidUri(e) => Some(e),
			Error::InvalidRequest(e) => Some(e),
			Error::ResolutionFailed => None,
			Error::ChannelFailure(e) => Some(e)
		}
    }
}

impl From<hyper::Error> for Error {
	fn from(val: hyper::Error) -> Self {
		Error::Hyper(val)
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
		Error::ResolutionFailed
	}
}

impl From<tokio::sync::mpsc::error::SendError<hyper::client::connect::dns::Name>> for Error {
	fn from(val: tokio::sync::mpsc::error::SendError<hyper::client::connect::dns::Name>) -> Self {
		Error::ChannelFailure(val)
	}
}

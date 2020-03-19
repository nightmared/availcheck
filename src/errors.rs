
#[derive(Debug)]
pub enum Error {
	Hyper(hyper::Error),
	Serde(serde_yaml::Error),
	Timeout(tokio::time::Elapsed)
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
			Error::Timeout(e) => Some(e)
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


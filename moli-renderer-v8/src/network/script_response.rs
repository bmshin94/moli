use std::{fmt, sync::Arc};

use moli_fetch::{Response, ResponseHead};
use moli_page_types::SubresourceResponseBody;

#[derive(Clone, Debug)]
pub(crate) struct ScriptResponseHead {
    pub(crate) head: ResponseHead,
    pub(crate) network_request_headers: Option<Vec<(String, String)>>,
}

/// A failed stream still owns the response facts already received. Cache
/// consumers admitted near completion must not lose its head or partial body.
#[derive(Clone, Debug)]
pub(crate) enum ScriptResponseFailure {
    Request(String),
    PartialBody {
        message: String,
        response: Arc<ScriptResponseHead>,
        body: SubresourceResponseBody,
    },
}

impl fmt::Display for ScriptResponseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(message) | Self::PartialBody { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for ScriptResponseFailure {}

impl From<String> for ScriptResponseFailure {
    fn from(message: String) -> Self {
        Self::Request(message)
    }
}

impl From<anyhow::Error> for ScriptResponseFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Request(format!("{error:#}"))
    }
}

pub(crate) type ScriptResponseResult = Result<Response, ScriptResponseFailure>;

/// Synchronous native fact publication only: implementations must not execute
/// script, invoke completion callbacks, or re-enter the resource cache. This
/// lets cache admission replay its current progress before a later chunk wins.
pub(crate) trait ScriptResponseObserver: Send + Sync {
    fn response_started(&self, response: Arc<ScriptResponseHead>);
    fn data_received(&self, bytes: usize);
}

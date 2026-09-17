//! Shared libcurl multi scheduler for Moli network requests.

mod dns_adapter;
mod http;
mod network_policy;
mod proxy;
mod runtime;
mod tls;
pub mod websocket;

pub use dns_adapter::CurlDnsResolution;
pub use http::{CurlHttpSender, CurlMultiCompletion, CurlMultiJob, CurlOriginKey, CurlSubmitError};
pub use network_policy::NetworkAddressPolicy;
pub use proxy::{
    ProxyRoute, ProxyScheme, ProxyTargetResolution, SelectedProxy, select_proxy_route,
    select_proxy_route_with_env,
};
pub use runtime::{CurlMultiRuntime, CurlMultiRuntimeConfig, CurlTransferId};
pub use tls::CurlTlsConfig;

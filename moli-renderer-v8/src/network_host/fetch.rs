mod bindings;
mod input;
mod promise;

use super::request::parse_fetch_init;
use super::*;

pub(crate) use self::bindings::window_fetch_callback;

/// Fetch options captured before interception and retained through auth retries.
/// URL, method, headers and body remain in the request's existing mutable state.
#[derive(Clone)]
pub(crate) struct WindowFetchOptions {
    pub(crate) metadata: crate::service_worker_runtime::ServiceWorkerFetchRequestMetadata,
    pub(crate) redirect_mode: moli_fetch::RequestRedirectMode,
    pub(crate) priority: Option<moli_fetch::FetchPriorityHint>,
    pub(crate) document_referrer_policy: Option<String>,
}

impl WindowFetchOptions {
    pub(crate) fn apply(&self, mut request: moli_fetch::Request) -> moli_fetch::Request {
        use moli_fetch::{RequestCacheMode, ScriptFetchRequestMetadata};

        let cache_mode = match self.metadata.cache.as_str() {
            "no-store" => RequestCacheMode::NoStore,
            "no-cache" | "reload" => RequestCacheMode::Validate,
            _ => RequestCacheMode::Default,
        };
        request = request
            .with_redirect_mode(self.redirect_mode)
            .with_cache_mode(cache_mode)
            .with_fetch_priority_hint(self.priority);
        if self.metadata.referrer.is_empty() {
            request = request.without_inferred_referrer();
        }
        let referrer_policy = (!self.metadata.referrer_policy.is_empty())
            .then(|| self.metadata.referrer_policy.clone());
        let integrity =
            (!self.metadata.integrity.is_empty()).then(|| self.metadata.integrity.clone());
        if referrer_policy.is_some()
            || self.document_referrer_policy.is_some()
            || integrity.is_some()
        {
            request = request.with_script_fetch_metadata(ScriptFetchRequestMetadata {
                referrer_policy,
                document_referrer_policy: self.document_referrer_policy.clone(),
                integrity,
                ..ScriptFetchRequestMetadata::default()
            });
        }
        request
    }
}

#[cfg(test)]
impl Default for WindowFetchOptions {
    fn default() -> Self {
        Self {
            metadata: Default::default(),
            redirect_mode: moli_fetch::RequestRedirectMode::Follow,
            priority: None,
            document_referrer_policy: None,
        }
    }
}

use std::{fmt, sync::Arc};

use super::ResourceTransfer;
use moli_fetch::{Response, ResponseHead};
use moli_page_types::SubresourceResponseBody;

#[derive(Clone, Debug)]
pub(crate) struct ResourceResponseHead {
    pub(crate) status_text: Option<String>,
    pub(crate) head: ResponseHead,
    pub(crate) network_request_headers: Option<Vec<(String, String)>>,
}

/// A failed stream still owns the response facts already received. Cache
/// consumers admitted near completion must not lose its head or partial body.
#[derive(Clone, Debug)]
pub(crate) enum ResourceResponseFailure {
    Request(String),
    PartialBody {
        message: String,
        response: Arc<ResourceResponseHead>,
        body: SubresourceResponseBody,
    },
}

impl ResourceResponseFailure {
    pub(crate) fn with_message(mut self, replacement: String) -> Self {
        match &mut self {
            Self::Request(message) | Self::PartialBody { message, .. } => *message = replacement,
        }
        self
    }
}

impl fmt::Display for ResourceResponseFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(message) | Self::PartialBody { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for ResourceResponseFailure {}

impl From<String> for ResourceResponseFailure {
    fn from(message: String) -> Self {
        Self::Request(message)
    }
}

impl From<anyhow::Error> for ResourceResponseFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Request(format!("{error:#}"))
    }
}

pub(crate) type ResourceResponseResult = Result<Response, ResourceResponseFailure>;

/// Synchronous native fact publication only: implementations must not execute
/// script, invoke completion callbacks, or re-enter the resource cache. This
/// lets cache admission replay its current progress before a later chunk wins.
pub(crate) trait ResourceResponseObserver: Send + Sync {
    fn response_started(&self, response: Arc<ResourceResponseHead>);
    fn data_received(&self, bytes: usize);
}

/// Preserve the physical response, including the received prefix on failure.
pub(crate) async fn collect_observed_response(
    observed: moli_fetch::NetworkFetchResult<moli_fetch::StreamingRawResponse>,
    observer: Option<&dyn ResourceResponseObserver>,
) -> ResourceResponseResult {
    let (mut response, request_observation) = observed.into_parts();
    let head = Arc::new(ResourceResponseHead {
        status_text: None,
        head: response.head(),
        network_request_headers: request_observation.map(|request| request.into_headers()),
    });
    if let Some(observer) = observer {
        observer.response_started(head.clone());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.next_chunk().await {
        bytes.extend_from_slice(&chunk);
        if let Some(observer) = observer {
            observer.data_received(chunk.len());
        }
    }
    if let Err(error) = response.finish().await {
        return Err(ResourceResponseFailure::PartialBody {
            message: format!("{error:#}"),
            response: head,
            body: moli_page_types::SubresourceResponseBody::from_bytes(bytes),
        });
    }
    Ok(
        moli_fetch::RawResponse::from_head_and_body(head.head.clone(), bytes)
            .into_lossy_materialized_text_response(),
    )
}

#[derive(Clone, Debug)]
pub(crate) struct ResourceBodyResponse {
    pub(crate) head: ResponseHead,
    pub(crate) body: SubresourceResponseBody,
}

impl From<Response> for ResourceBodyResponse {
    fn from(response: Response) -> Self {
        Self {
            head: response.head(),
            body: SubresourceResponseBody::from_fetch_response(&response),
        }
    }
}

impl ResourceBodyResponse {
    pub(crate) fn head(&self) -> ResponseHead {
        self.head.clone()
    }

    pub(crate) fn subresource_response_body(&self) -> SubresourceResponseBody {
        self.body.clone()
    }

    pub(crate) fn publish(
        &self,
        network: &ResourceTransfer,
        network_request_headers: Option<Vec<(String, String)>>,
    ) {
        network.body_completed(
            ResourceResponseHead {
                status_text: None,
                head: self.head.clone(),
                network_request_headers,
            },
            self.body.clone(),
        );
    }

    pub(crate) fn failure(
        &self,
        message: String,
        network_request_headers: Option<Vec<(String, String)>>,
    ) -> ResourceResponseFailure {
        ResourceResponseFailure::PartialBody {
            message,
            response: Arc::new(ResourceResponseHead {
                status_text: None,
                head: self.head.clone(),
                network_request_headers,
            }),
            body: self.body.clone(),
        }
    }

    pub(crate) fn body_source(&self) -> Result<moli_fetch::ResponseBody, String> {
        self.body
            .materialize_bytes()
            .map(moli_fetch::ResponseBody::materialized_bytes)
            .map_err(|error| format!("failed to materialize worker XHR body: {error}"))
    }
}

impl From<moli_page_types::NavigationResponse> for ResourceBodyResponse {
    fn from(response: moli_page_types::NavigationResponse) -> Self {
        Self {
            head: response.head(),
            body: SubresourceResponseBody::from_navigation_response(&response),
        }
    }
}

impl ResourceBodyResponse {
    pub(crate) fn into_navigation_response(
        self,
    ) -> Result<moli_page_types::NavigationResponse, String> {
        let body = self
            .body
            .materialize_bytes()
            .map_err(|error| format!("failed to materialize resource body: {error}"))?;
        Ok(
            moli_page_types::NavigationResponse::from_head_and_materialized_body(
                self.head,
                moli_fetch::ResponseBody::materialized_bytes(body),
            ),
        )
    }
}

/// The physical producer and its consumer share received bytes. A cancellation
/// can retain the exact prefix even before queued JS callbacks have run.
/// Response decisions and the first wire request headers belong to this same resource.
pub(crate) struct ResourceResponseStream {
    pub(crate) network: Arc<ResourceTransfer>,
    response: parking_lot::Mutex<ResourceResponseState>,
    window_fetch_policy: Option<Box<crate::network_host::WindowFetchResponsePolicy>>,
}

#[derive(Default)]
struct ResourceResponseState {
    body: ResourceStreamBody,
    intercept_response: bool,
    handle_auth_requests: bool,
    network_request_headers: Option<Vec<(String, String)>>,
}

impl ResourceResponseState {
    fn intercepts(&self, head: &ResponseHead) -> bool {
        self.intercept_response
            || (self.handle_auth_requests
                && matches!(head.status, 401 | 407)
                && crate::network_host::extract_subresource_auth_challenge(&head.headers).is_some())
    }

    fn record_request_headers(
        &mut self,
        headers: Option<Vec<(String, String)>>,
    ) -> Option<Vec<(String, String)>> {
        // Authentication retries keep the original browser request's wire headers.
        if self.network_request_headers.is_none() {
            self.network_request_headers = headers;
        }
        self.network_request_headers.clone()
    }
}

#[derive(Default)]
enum ResourceStreamBody {
    #[default]
    Pending,
    Reading(
        Arc<ResourceResponseHead>,
        moli_page_types::SubresourceResponseBodyWriter,
    ),
    Paused(
        Arc<ResourceResponseHead>,
        moli_page_types::SubresourceResponseBodyWriter,
    ),
    Received(Arc<ResourceResponseHead>, SubresourceResponseBody),
}

impl ResourceResponseStream {
    pub(crate) fn new(network: Arc<ResourceTransfer>) -> Arc<Self> {
        Arc::new(Self {
            network,
            response: Default::default(),
            window_fetch_policy: None,
        })
    }

    pub(crate) fn for_window_fetch(
        network: Arc<ResourceTransfer>,
        connect_policy: crate::document_runtime::DocumentConnectPolicySnapshot,
        report_context: crate::network_host::WindowCspReportRequestContext,
    ) -> Arc<Self> {
        Arc::new(Self {
            network,
            response: Default::default(),
            window_fetch_policy: Some(Box::new(
                crate::network_host::WindowFetchResponsePolicy::new(connect_policy, report_context),
            )),
        })
    }

    pub(crate) fn window_fetch_policy(
        &self,
    ) -> Option<&crate::network_host::WindowFetchResponsePolicy> {
        self.window_fetch_policy.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn unobserved_for_test() -> Arc<Self> {
        let url = url::Url::parse("https://resource.test/").unwrap();
        let (network, _) = ResourceTransfer::start(
            crate::runtime::RendererNetworkRequest::unobserved_for_test(),
            |_| {},
            |request| {
                moli_page_types::SubresourceRequestStarted::new(
                    request.handle(),
                    None,
                    url.clone(),
                    url,
                    "GET".into(),
                    Vec::new(),
                    None,
                    moli_page_types::SubresourceResourceType::Fetch,
                    moli_page_types::SubresourceRequestInitiatorType::Other,
                    None,
                )
            },
        );
        Self::new(network)
    }

    pub(crate) fn configure_interception(
        &self,
        intercept_response: bool,
        handle_auth_requests: bool,
    ) {
        let mut state = self.response.lock();
        state.intercept_response = intercept_response;
        state.handle_auth_requests = handle_auth_requests;
    }

    pub(crate) fn intercept_response(&self) -> bool {
        self.response.lock().intercept_response
    }

    pub(crate) fn handle_auth_requests(&self) -> bool {
        self.response.lock().handle_auth_requests
    }

    pub(crate) fn intercepts_response(&self, head: &ResponseHead) -> bool {
        self.response.lock().intercepts(head)
    }

    pub(crate) fn record_request_headers(
        &self,
        headers: Option<Vec<(String, String)>>,
    ) -> Option<Vec<(String, String)>> {
        self.response.lock().record_request_headers(headers)
    }

    pub(crate) fn response_started(&self, mut response: ResourceResponseHead) {
        let mut state = self.response.lock();
        response.network_request_headers =
            state.record_request_headers(response.network_request_headers);
        let response = Arc::new(response);
        if state.intercepts(&response.head) {
            state.body = ResourceStreamBody::Paused(response, Default::default());
        } else {
            state.body = ResourceStreamBody::Reading(response.clone(), Default::default());
            drop(state);
            self.network.response_started(response);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_body_writer_for_test(
        &self,
        writer: moli_page_types::SubresourceResponseBodyWriter,
    ) {
        let mut response = self.response.lock();
        let (ResourceStreamBody::Reading(_, body) | ResourceStreamBody::Paused(_, body)) =
            &mut response.body
        else {
            panic!("test body requires an admitted response head")
        };
        *body = writer;
    }

    pub(crate) fn data_received(&self, bytes: &[u8]) {
        let mut response = self.response.lock();
        match &mut response.body {
            ResourceStreamBody::Reading(_, body) => {
                body.append(bytes);
                self.network.data_received(bytes.len());
            }
            ResourceStreamBody::Paused(_, body) => body.append(bytes),
            ResourceStreamBody::Pending | ResourceStreamBody::Received(..) => {}
        }
    }

    pub(crate) fn finish_response(&self) -> Option<ResourceBodyResponse> {
        let mut state = self.response.lock();
        if matches!(
            state.body,
            ResourceStreamBody::Reading(..) | ResourceStreamBody::Paused(..)
        ) {
            let (ResourceStreamBody::Reading(head, body) | ResourceStreamBody::Paused(head, body)) =
                std::mem::replace(&mut state.body, ResourceStreamBody::Pending)
            else {
                unreachable!()
            };
            state.body = ResourceStreamBody::Received(head, body.finish());
        }
        match &state.body {
            ResourceStreamBody::Received(head, body) => Some(ResourceBodyResponse {
                head: head.head.clone(),
                body: body.clone(),
            }),
            ResourceStreamBody::Pending => None,
            ResourceStreamBody::Reading(..) | ResourceStreamBody::Paused(..) => unreachable!(),
        }
    }

    pub(crate) fn failure(&self, message: String) -> ResourceResponseFailure {
        self.finish_response();
        match &self.response.lock().body {
            ResourceStreamBody::Received(response, body) => ResourceResponseFailure::PartialBody {
                message,
                response: response.clone(),
                body: body.clone(),
            },
            ResourceStreamBody::Pending => ResourceResponseFailure::Request(message),
            ResourceStreamBody::Reading(..) | ResourceStreamBody::Paused(..) => unreachable!(),
        }
    }
}

impl Drop for ResourceResponseStream {
    fn drop(&mut self) {
        if !matches!(self.response.get_mut().body, ResourceStreamBody::Pending) {
            self.network
                .failed(&self.failure("Resource load cancelled".into()));
        }
    }
}

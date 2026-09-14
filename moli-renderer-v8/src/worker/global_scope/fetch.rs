use super::*;
use crate::service_worker_runtime::{
    ServiceWorkerClientId, ServiceWorkerFetchDispatch, ServiceWorkerFetchRequest,
    ServiceWorkerFetchRequestMetadata, ServiceWorkerFetchResultSender,
    ServiceWorkerRequestDestination, ServiceWorkerRuntimeService,
};
use crate::types::AsyncSubresourceNetworkContext;
use moli_page_types::{SubresourceRequestInitiatorType, SubresourceRequestStarted};

mod response;
pub(in crate::worker) use response::{WorkerFetchAuthResponse, WorkerFetchPausedResponse};
pub(crate) use response::{WorkerFetchCompletionSender, WorkerFetchStreamSender};

#[cfg(test)]
mod tests;

impl Drop for WorkerRequestDelivery {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            completion.publish(&self.response);
        }
    }
}

fn worker_fetch_request_metadata(
    pending: &PendingWorkerFetch,
) -> (&Url, &str, &[(String, String)], &Option<Vec<u8>>) {
    match &pending.request_override {
        Some(record) => (
            &record.url,
            &record.method,
            &record.request_headers,
            &record.request_body,
        ),
        None => (
            &pending.request_url,
            &pending.request_method,
            &pending.request_headers,
            &pending.request_body,
        ),
    }
}

fn worker_fetch_request(
    pending: &PendingWorkerFetch,
    network: &crate::runtime::RendererNetworkRequest,
) -> SubresourceRequestStarted {
    let (url, method, headers, body) = worker_fetch_request_metadata(pending);
    worker_request_started(
        network,
        &pending.document_url,
        url,
        method,
        headers,
        body,
        SubresourceResourceType::Fetch,
    )
    .with_keepalive(pending.request_metadata.keepalive)
}

pub(in crate::worker) fn worker_request_started(
    network: &crate::runtime::RendererNetworkRequest,
    document_url: &Url,
    url: &Url,
    method: &str,
    headers: &[(String, String)],
    body: &Option<Vec<u8>>,
    resource_type: SubresourceResourceType,
) -> SubresourceRequestStarted {
    SubresourceRequestStarted::new(
        network.handle(),
        None,
        document_url.clone(),
        url.clone(),
        method.to_owned(),
        headers.to_vec(),
        request_body_text(body),
        resource_type,
        SubresourceRequestInitiatorType::Script,
        None,
    )
    .with_request_body_bytes(body.clone())
}

fn update_worker_fetch_request(pending: &mut PendingWorkerFetch, record: WorkerRequestOverride) {
    let (url, method, headers, body) = worker_fetch_request_metadata(pending);
    let changed = url != &record.url
        || method != record.method
        || headers != record.request_headers
        || body != &record.request_body;
    pending.request_override = Some(record);
    if changed {
        pending
            .response
            .network
            .update_request(|network| worker_fetch_request(pending, network));
    }
}

pub(in crate::worker) fn record_worker_subresource_failure(
    state: &WorkerGlobalState,
    document_url: Url,
    url: Url,
    method: String,
    request_headers: Vec<(String, String)>,
    request_body: Option<Vec<u8>>,
    resource_type: SubresourceResourceType,
    error_text: String,
) {
    let Some(transfer) = crate::network::ResourceTransfer::for_worker(
        state.global_kind.network(),
        state.parent_tx.network_observer(),
        |network| {
            worker_request_started(
                network,
                &document_url,
                &url,
                &method,
                &request_headers,
                &request_body,
                resource_type,
            )
        },
    ) else {
        return;
    };
    transfer.failed(&crate::network::ResourceResponseFailure::Request(
        error_text,
    ));
}

fn worker_network_result_parts<R>(
    observed: moli_fetch::NetworkFetchResult<R>,
) -> (R, Option<Vec<(String, String)>>) {
    let (response, request_observation) = observed.into_parts();
    (
        response,
        request_observation.map(|observation| observation.into_headers()),
    )
}

async fn collect_worker_resource_response(
    observed: moli_fetch::NetworkFetchResult<moli_fetch::StreamingRawResponse>,
    resource: &ResourceResponseStream,
    error_prefix: &str,
) -> (
    Result<ResourceBodyResponse, ResourceResponseFailure>,
    Option<Vec<(String, String)>>,
) {
    let (mut response, network_request_headers) = worker_network_result_parts(observed);
    resource.response_started(ResourceResponseHead {
        status_text: None,
        head: response.head(),
        network_request_headers: network_request_headers.clone(),
    });
    while let Some(chunk) = response.next_chunk().await {
        resource.data_received(&chunk);
    }
    let result = match response.finish().await {
        Ok(()) => Ok(resource.finish_response().expect("received response")),
        Err(error) => Err(resource.failure(format!("{error_prefix}: {error}"))),
    };
    (result, network_request_headers)
}

fn worker_fetch_network_request(
    state: &WorkerGlobalState,
    pending: &PendingWorkerFetch,
    auth: Option<crate::protocol_types::SubresourceAuthCredentials>,
    suppress_default_content_type: bool,
) -> Result<Request, ResourceResponseFailure> {
    let (url, method, headers, body) = worker_fetch_request_metadata(pending);
    let mut headers = headers.to_vec();
    if suppress_default_content_type {
        // The empty value prevents the transport from synthesizing an upload default.
        headers.push(("Content-Type".to_owned(), String::new()));
    }
    let mut request = Request::new_bytes(method, url.as_str(), body.clone(), headers)
        .map_err(|error| {
            ResourceResponseFailure::Request(format!("fetch: failed to build request: {error}"))
        })?
        .with_initiator_url(&pending.document_url)
        .with_request_mode(pending.request_mode)
        .with_credentials_mode(pending.credentials_mode)
        .with_redirect_mode(pending.redirect_mode)
        .with_cache_mode(worker_fetch_cache_mode(&pending.request_metadata.cache))
        .with_fetch_priority_hint(pending.request_priority)
        .with_network_partition_key(state.network_partition_key.clone())
        .with_browser_request_metadata(BrowserRequestMetadata::Fetch);
    if pending.request_metadata.referrer.is_empty() {
        request = request.without_inferred_referrer();
    }
    if let Some(metadata) =
        worker_fetch_script_metadata(state.referrer_policy.clone(), &pending.request_metadata)
    {
        request = request.with_script_fetch_metadata(metadata);
    }
    if let Some(auth) = auth {
        request = request.with_auth(auth.into());
    }
    Ok(request)
}

fn start_worker_fetch(
    state: &Rc<RefCell<WorkerGlobalState>>,
    fetch_id: u32,
    cancel: FetchCancelHandle,
    auth: Option<crate::protocol_types::SubresourceAuthCredentials>,
    suppress_default_content_type: bool,
    controller: Option<(ServiceWorkerRuntimeService, ServiceWorkerClientId)>,
) {
    let state = state.borrow();
    let pending = &state.pending_fetches[&fetch_id];
    let sender = WorkerFetchCompletionSender::new(&state, pending, fetch_id);
    let request =
        match worker_fetch_network_request(&state, pending, auth, suppress_default_content_type) {
            Ok(request) => request,
            Err(error) => {
                sender.complete(Err(error), None);
                return;
            }
        };
    let (_, _, headers, _) = worker_fetch_request_metadata(pending);
    if let Some((runtime, client_id)) = controller {
        let dispatch = ServiceWorkerFetchDispatch {
            internal_id: u64::from(fetch_id),
            request: ServiceWorkerFetchRequest {
                client_id,
                resulting_client_id: None,
                url: request.url.clone(),
                method: request.method.clone(),
                headers: headers.to_vec(),
                body: request.body.clone(),
                destination: ServiceWorkerRequestDestination::Empty,
                request_mode: request.request_mode,
                credentials_mode: request.credentials_mode,
                redirect_mode: request.redirect_mode,
                priority: request.priority_hints.fetch_priority,
                is_reload: false,
                metadata: pending.request_metadata.clone(),
            },
            request_body_text: request_body_text(&request.body),
            cors_preflight_request_headers: headers.to_vec(),
            request_cookie_report: None,
            network_context: AsyncSubresourceNetworkContext {
                frame_id: None,
                document_url: pending.document_url.clone(),
                resource_type: SubresourceResourceType::Fetch,
                policy_context: pending.policy_context,
            },
            result_tx: ServiceWorkerFetchResultSender::Worker {
                sender: Box::new(sender),
                request: Box::new(request),
            },
            request_client: pending.load.request_client(),
            resource_task_runner: pending.load.task_runner(),
            cancel_handle: cancel,
        };
        let load = pending.load.clone();
        let response = pending.response.clone();
        drop(state);
        runtime.dispatch_controlled_fetch(dispatch);
        // Bind after admission: a retirement that already canceled the lease
        // invokes the hook immediately, with the service job now addressable.
        runtime.attach_fetch_cancellation(&load, &response);
    } else {
        let headers = headers.to_vec();
        drop(state);
        sender.fetch_network(request, cancel, headers, Vec::new());
    }
}

fn worker_fetch_cache_mode(cache: &str) -> moli_fetch::RequestCacheMode {
    match cache {
        "no-cache" | "no-store" | "reload" => moli_fetch::RequestCacheMode::Validate,
        _ => moli_fetch::RequestCacheMode::Default,
    }
}

fn worker_fetch_script_metadata(
    document_referrer_policy: Option<String>,
    request_metadata: &ServiceWorkerFetchRequestMetadata,
) -> Option<moli_fetch::ScriptFetchRequestMetadata> {
    let referrer_policy = (!request_metadata.referrer_policy.is_empty())
        .then(|| request_metadata.referrer_policy.clone());
    let integrity =
        (!request_metadata.integrity.is_empty()).then(|| request_metadata.integrity.clone());
    if referrer_policy.is_none() && document_referrer_policy.is_none() && integrity.is_none() {
        return None;
    }
    Some(moli_fetch::ScriptFetchRequestMetadata {
        referrer_policy,
        document_referrer_policy,
        integrity,
        ..moli_fetch::ScriptFetchRequestMetadata::default()
    })
}

fn worker_service_worker_controller(
    state: &WorkerGlobalState,
    resolved_url: &Url,
) -> Option<(ServiceWorkerRuntimeService, ServiceWorkerClientId)> {
    if !matches!(resolved_url.scheme(), "http" | "https") {
        return None;
    }
    let runtime = state.service_worker_runtime.clone()?;
    let client_id = state.service_worker_client_id?;
    runtime.matching_controller_for_client_fetch(client_id, resolved_url)?;
    Some((runtime, client_id))
}

pub(in crate::worker) fn spawn_worker_xhr_network(
    load: ResourceLoadLease,
    resource: Arc<ResourceResponseStream>,
    observer: crate::worker::WorkerNetworkObserver,
    deliver: impl FnOnce(WorkerRequestDelivery) + Send + 'static,
    xhr_id: u32,
    cancel_handle: FetchCancelHandle,
    request: Result<Request, String>,
) {
    let preflight_network = resource.network.request();
    load.task_runner().spawn(async move {
        let loader = load.request_client();
        let preflight_observer = observer.clone();
        let preflight = crate::network_host::CorsPreflightNetworkObserver {
            request: preflight_network,
            observer: std::sync::Arc::new(move |event| preflight_observer.publish(event)),
            frame_id: None,
            resource_type: SubresourceResourceType::Xhr,
            keepalive: false,
        };
        let cors_preflight_request_headers = request
            .as_ref()
            .map(|request| request.request_headers.clone())
            .unwrap_or_default();

        let (result, network_request_headers) = match request {
            Ok(request) => {
                // Ordinary worker XHR can keep the network/cache path streaming
                // and spool large bodies until the XHR DONE boundary.
                match fetch_browser_subresource_raw_stream_with_preflight_headers_and_observer(
                    &loader,
                    request,
                    Some(cancel_handle),
                    cors_preflight_request_headers,
                    Some(&preflight),
                )
                .await
                {
                    Ok(observed) => {
                        collect_worker_resource_response(observed, &resource, "xhr").await
                    }
                    Err(error) => {
                        let message = format!("xhr: {error}");
                        (Err(error.with_message(message)), None)
                    }
                }
            }
            Err(error) => (Err(ResourceResponseFailure::from(error)), None),
        };
        if !result
            .as_ref()
            .is_ok_and(|response| resource.intercepts_response(&response.head))
        {
            load.finish();
        }
        deliver(WorkerRequestDelivery::new(
            resource,
            WorkerRequestCompletion {
                id: xhr_id,
                network_request_headers,
                result,
            },
        ));
    });
}

pub(in crate::worker) fn continue_pending_worker_fetch(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingFetchContinue,
) {
    let cancel = FetchCancelHandle::new();
    {
        let mut state = state.borrow_mut();
        let Some(pending) = state.pending_fetches.get_mut(&request.fetch_id) else {
            return;
        };
        if let Some(response) = pending.paused_response.take() {
            response.discard();
        }
        pending.load.attach_cancel_handle(cancel.clone());
        pending
            .response
            .configure_interception(request.intercept_response, request.handle_auth_requests);
        update_worker_fetch_request(
            pending,
            WorkerRequestOverride {
                url: request.url,
                method: request.method,
                request_headers: request.headers,
                request_body: request.body,
            },
        );
    }
    start_worker_fetch(state, request.fetch_id, cancel, request.auth, false, None);
}

pub(in crate::worker) fn fail_pending_worker_fetch(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingFetchContinue,
    error_text: String,
) {
    let fetch_id = request.fetch_id;
    let completion_tx = {
        let mut state = state.borrow_mut();
        let completion_tx = state.fetch_completion_tx.clone();
        if let Some(pending) = state.pending_fetches.get_mut(&fetch_id) {
            pending.response.configure_interception(false, false);
            update_worker_fetch_request(
                pending,
                WorkerRequestOverride {
                    url: request.url,
                    method: request.method,
                    request_headers: request.headers,
                    request_body: request.body,
                },
            );
        }
        completion_tx
    };
    let _ = completion_tx.send(WorkerFetchEvent::Completion(Box::new(
        WorkerRequestCompletion {
            id: fetch_id,
            network_request_headers: None,
            result: Err(error_text.into()),
        },
    )));
}

pub(in crate::worker) fn fail_pending_worker_fetch_auth(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingFetchContinue,
    error_text: String,
) {
    let fetch_id = request.fetch_id;
    let completion_tx = {
        let mut state = state.borrow_mut();
        let completion_tx = state.fetch_completion_tx.clone();
        if let Some(pending) = state.pending_fetches.get_mut(&fetch_id) {
            pending.response.configure_interception(false, false);
            if let Some(response) = pending.paused_response.take() {
                response.discard();
            }
        }
        completion_tx
    };
    let _ = completion_tx.send(WorkerFetchEvent::Completion(Box::new(
        WorkerRequestCompletion {
            id: fetch_id,
            network_request_headers: None,
            result: Err(error_text.into()),
        },
    )));
}

pub(in crate::worker) fn fulfill_pending_worker_fetch(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingFetchContinue,
    response_code: u16,
    response_headers: Vec<(String, String)>,
    response_body: RendererSyntheticResponseBody,
) {
    let fetch_id = request.fetch_id;
    let completion = {
        let mut state = state.borrow_mut();
        let completion_tx = state.fetch_completion_tx.clone();
        let Some(pending) = state.pending_fetches.get_mut(&fetch_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        update_worker_fetch_request(
            pending,
            WorkerRequestOverride {
                url: request.url.clone(),
                method: request.method.clone(),
                request_headers: request.headers.clone(),
                request_body: request.body.clone(),
            },
        );
        let response =
            worker_response_from_body(request.url, response_code, response_headers, response_body);
        (completion_tx, response, fetch_id)
    };
    let _ = completion.0.send(WorkerFetchEvent::Completion(Box::new(
        WorkerRequestCompletion {
            id: completion.2,
            network_request_headers: None,
            result: Ok(ResourceBodyResponse::from(completion.1)),
        },
    )));
}

pub(in crate::worker) fn continue_pending_worker_fetch_response(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingFetchContinue,
    response_code: Option<u16>,
    response_headers: Option<Vec<(String, String)>>,
) {
    let fetch_id = request.fetch_id;
    let completion = {
        let mut state = state.borrow_mut();
        let completion_tx = state.fetch_completion_tx.clone();
        let Some(pending) = state.pending_fetches.get_mut(&fetch_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        let Some(response) = pending.paused_response.take() else {
            return;
        };
        (completion_tx, response, fetch_id)
    };
    completion
        .1
        .resume(&completion.0, completion.2, response_code, response_headers);
}

pub(in crate::worker) fn fail_pending_worker_fetch_response(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingFetchContinue,
    error_text: String,
) {
    let fetch_id = request.fetch_id;
    let completion_tx = {
        let mut state = state.borrow_mut();
        let completion_tx = state.fetch_completion_tx.clone();
        let Some(pending) = state.pending_fetches.get_mut(&fetch_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        if let Some(response) = pending.paused_response.take() {
            response.discard();
        }
        completion_tx
    };
    let _ = completion_tx.send(WorkerFetchEvent::Completion(Box::new(
        WorkerRequestCompletion {
            id: fetch_id,
            network_request_headers: None,
            result: Err(error_text.into()),
        },
    )));
}

pub(in crate::worker) fn fulfill_pending_worker_fetch_response(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingFetchContinue,
    response_code: u16,
    response_headers: Vec<(String, String)>,
    response_body: RendererSyntheticResponseBody,
) {
    let fetch_id = request.fetch_id;
    let completion = {
        let mut state = state.borrow_mut();
        let completion_tx = state.fetch_completion_tx.clone();
        let Some(pending) = state.pending_fetches.get_mut(&fetch_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        let Some(paused) = pending.paused_response.take() else {
            return;
        };
        let mut head = paused.discard();
        head.status = response_code;
        head.headers = response_headers;
        head.cookie_set_reports.clear();
        let response = response_body.into_fetch_response(head);
        (completion_tx, response, fetch_id)
    };
    let _ = completion.0.send(WorkerFetchEvent::Completion(Box::new(
        WorkerRequestCompletion {
            id: completion.2,
            network_request_headers: None,
            result: Ok(ResourceBodyResponse::from(completion.1)),
        },
    )));
}

pub(in crate::worker) fn continue_pending_worker_xhr(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingXhrContinue,
) {
    let (load, resource, observer, completion_tx, cancel_handle, prepared, xhr_id, auth) = {
        let mut state = state.borrow_mut();
        let completion_tx = state.xhr_completion_tx.clone();
        let observer = state.parent_tx.network_observer();
        let Some(pending) = state.pending_xhrs.get_mut(&request.xhr_id) else {
            return;
        };
        let cancel_handle = FetchCancelHandle::new();
        pending.load.attach_cancel_handle(cancel_handle.clone());
        pending
            .response
            .configure_interception(request.intercept_response, request.handle_auth_requests);
        super::xhr::update_worker_xhr_request(
            pending,
            WorkerRequestOverride {
                url: request.url.clone(),
                method: request.method.clone(),
                request_headers: request.headers.clone(),
                request_body: request.body.clone(),
            },
        );
        (
            pending.load.clone(),
            pending.response.clone(),
            observer,
            completion_tx,
            cancel_handle,
            super::xhr::PreparedWorkerXhrSendRequest {
                document_url: pending.document_url.clone(),
                resolved_url: request.url,
                method: request.method,
                send_body: request.body,
                request_headers: request.headers,
                credentials_mode: pending.credentials_mode,
            },
            request.xhr_id,
            request.auth,
        )
    };

    spawn_worker_xhr_network(
        load,
        resource,
        observer,
        move |delivery| {
            let _ = completion_tx.send(WorkerXhrCompletion::TransportCompletion(delivery));
        },
        xhr_id,
        cancel_handle,
        prepared
            .network_request(&state.borrow())
            .map(|request| match auth {
                Some(auth) => request.with_auth(auth.into()),
                None => request,
            }),
    );
}

pub(in crate::worker) fn fail_pending_worker_xhr(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingXhrContinue,
    error_text: String,
) {
    let xhr_id = request.xhr_id;
    let completion_tx = {
        let mut state = state.borrow_mut();
        let completion_tx = state.xhr_completion_tx.clone();
        if let Some(pending) = state.pending_xhrs.get_mut(&xhr_id) {
            pending.response.configure_interception(false, false);
            super::xhr::update_worker_xhr_request(
                pending,
                WorkerRequestOverride {
                    url: request.url,
                    method: request.method,
                    request_headers: request.headers,
                    request_body: request.body,
                },
            );
        }
        completion_tx
    };
    let _ = completion_tx.send(WorkerXhrCompletion::decision(
        xhr_id,
        Err(error_text.into()),
    ));
}

pub(in crate::worker) fn fail_pending_worker_xhr_auth(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingXhrContinue,
    error_text: String,
) {
    let xhr_id = request.xhr_id;
    let completion_tx = {
        let mut state = state.borrow_mut();
        let completion_tx = state.xhr_completion_tx.clone();
        if let Some(pending) = state.pending_xhrs.get_mut(&xhr_id) {
            pending.response.configure_interception(false, false);
            pending.paused_response = None;
        }
        completion_tx
    };
    let _ = completion_tx.send(WorkerXhrCompletion::decision(
        xhr_id,
        Err(error_text.into()),
    ));
}

pub(in crate::worker) fn fulfill_pending_worker_xhr(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingXhrContinue,
    response_code: u16,
    response_headers: Vec<(String, String)>,
    response_body: RendererSyntheticResponseBody,
) {
    let xhr_id = request.xhr_id;
    let completion = {
        let mut state = state.borrow_mut();
        let completion_tx = state.xhr_completion_tx.clone();
        let Some(pending) = state.pending_xhrs.get_mut(&xhr_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        super::xhr::update_worker_xhr_request(
            pending,
            WorkerRequestOverride {
                url: request.url.clone(),
                method: request.method.clone(),
                request_headers: request.headers.clone(),
                request_body: request.body.clone(),
            },
        );
        let response =
            worker_response_from_body(request.url, response_code, response_headers, response_body);
        (completion_tx, response, xhr_id)
    };
    let _ = completion.0.send(WorkerXhrCompletion::decision(
        completion.2,
        Ok(ResourceBodyResponse::from(completion.1)),
    ));
}

pub(in crate::worker) fn continue_pending_worker_xhr_response(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingXhrContinue,
    response_code: Option<u16>,
    response_headers: Option<Vec<(String, String)>>,
) {
    let xhr_id = request.xhr_id;
    let completion = {
        let mut state = state.borrow_mut();
        let completion_tx = state.xhr_completion_tx.clone();
        let Some(pending) = state.pending_xhrs.get_mut(&xhr_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        let Some(mut response) = pending.paused_response.take() else {
            return;
        };
        if let Some(response_code) = response_code {
            response.head.status = response_code;
        }
        if let Some(response_headers) = response_headers {
            response.head.headers = response_headers;
        }
        (completion_tx, response, xhr_id)
    };
    let _ = completion.0.send(WorkerXhrCompletion::decision(
        completion.2,
        Ok(completion.1),
    ));
}

pub(in crate::worker) fn fail_pending_worker_xhr_response(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingXhrContinue,
    error_text: String,
) {
    let xhr_id = request.xhr_id;
    let completion_tx = {
        let mut state = state.borrow_mut();
        let completion_tx = state.xhr_completion_tx.clone();
        let Some(pending) = state.pending_xhrs.get_mut(&xhr_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        pending.paused_response = None;
        completion_tx
    };
    let _ = completion_tx.send(WorkerXhrCompletion::decision(
        xhr_id,
        Err(error_text.into()),
    ));
}

pub(in crate::worker) fn fulfill_pending_worker_xhr_response(
    state: &Rc<RefCell<WorkerGlobalState>>,
    request: WorkerPendingXhrContinue,
    response_code: u16,
    response_headers: Vec<(String, String)>,
    response_body: RendererSyntheticResponseBody,
) {
    let xhr_id = request.xhr_id;
    let completion = {
        let mut state = state.borrow_mut();
        let completion_tx = state.xhr_completion_tx.clone();
        let Some(pending) = state.pending_xhrs.get_mut(&xhr_id) else {
            return;
        };
        pending.response.configure_interception(false, false);
        let Some(mut paused) = pending.paused_response.take() else {
            return;
        };
        paused.head.status = response_code;
        paused.head.headers = response_headers;
        paused.head.cookie_set_reports.clear();
        let response = response_body.into_fetch_response(paused.head);
        (completion_tx, response, xhr_id)
    };
    let _ = completion.0.send(WorkerXhrCompletion::decision(
        completion.2,
        Ok(ResourceBodyResponse::from(completion.1)),
    ));
}

pub(in crate::worker) fn record_worker_websocket_subresource_failure(
    state: &WorkerGlobalState,
    socket_id: u64,
    document_url: Url,
    url: Url,
    request_headers: Vec<(String, String)>,
    error_text: String,
) {
    let _ = state
        .parent_tx
        .send(WorkerToParentMessage::WebSocketSubresource(
            SubresourceNetworkRecord::failure(
                None,
                document_url,
                url,
                "GET".to_owned(),
                request_headers,
                None,
                SubresourceResourceType::WebSocket,
                error_text,
            )
            .with_websocket_socket_id(socket_id),
        ));
}

#[must_use = "the caller must stop mixed-content processing when worker CSP blocks the request"]
pub(crate) enum WorkerWebSocketCspOutcome {
    Allowed,
    Blocked(String),
}

impl WorkerWebSocketCspOutcome {
    pub(crate) fn blocks_request(&self) -> bool {
        matches!(self, Self::Blocked(_))
    }

    fn into_failure_message(self) -> Option<String> {
        match self {
            Self::Allowed => None,
            Self::Blocked(message) => Some(message),
        }
    }
}

pub(crate) fn check_worker_websocket_csp(
    scope: &mut v8::PinScope<'_, '_>,
    document_url: &Url,
    url: &Url,
) -> Option<WorkerWebSocketCspOutcome> {
    let state = get_worker_state(scope)?;
    dispatch_worker_content_security_policy_report_only_violation_for_state(
        scope,
        &state,
        document_url,
        url,
        crate::content_security_policy::ContentSecurityPolicyResourceKind::WorkerConnect,
    );
    let violation = {
        let state_ref = state.borrow();
        worker_content_security_policy_violation(
            &state_ref,
            document_url,
            url,
            crate::content_security_policy::ContentSecurityPolicyResourceKind::WorkerConnect,
        )
    };
    Some(match violation {
        Some(violation) => {
            let message = worker_content_security_policy_error_message(&violation, "WebSocket");
            dispatch_worker_content_security_policy_violation_event_for_state(
                scope, &state, &violation,
            );
            WorkerWebSocketCspOutcome::Blocked(message)
        }
        None => WorkerWebSocketCspOutcome::Allowed,
    })
}

pub(crate) fn register_worker_websocket<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    wrapper: v8::Local<'s, v8::Object>,
    document_url: Url,
    url: Url,
    protocols: Vec<String>,
    csp_outcome: WorkerWebSocketCspOutcome,
) -> Option<u64> {
    let state = get_worker_state(scope)?;
    let (
        socket_id,
        loader,
        extra_http_headers,
        network_offline,
        blocked_url_patterns,
        websocket_event_tx,
    ) = {
        let mut state = state.borrow_mut();
        let socket_id = next_websocket_id(&mut state);
        (
            socket_id,
            state.loader.clone(),
            state.extra_http_headers.clone(),
            state.network_offline,
            state.blocked_url_patterns.clone(),
            state.websocket_event_tx.clone(),
        )
    };

    let cookie_url = websocket_cookie_url(&url);
    let cookie_context = moli_cookie_jar::NetworkCookieRequestContext::subresource("GET")
        .with_initiator_url(&cookie_url, &document_url);
    let cookie_header = moli_fetch::cookie_header_for_request(
        &loader.request_client().cookie_store(),
        &cookie_url,
        cookie_context,
    );
    let cookie_header_for_context = cookie_header
        .as_ref()
        .ok()
        .and_then(|header| header.clone());
    let context = WebSocketConnectOptions {
        origin: moli_url::origin_ascii_serialization(&document_url),
        user_agent: loader.request_client().user_agent().to_owned(),
        extra_headers: extra_http_headers,
        http_proxy: loader.request_client().http_proxy().map(ToOwned::to_owned),
        http_no_proxy: loader
            .request_client()
            .http_no_proxy()
            .map(ToOwned::to_owned),
        proxy_bearer_token: loader
            .request_client()
            .proxy_bearer_token()
            .map(ToOwned::to_owned),
        tls: loader.request_client().tls_config().clone(),
        cookie_header: cookie_header_for_context,
    };

    let csp_failure = csp_outcome.into_failure_message();
    let blocked = worker_url_blocked(&blocked_url_patterns, &url);
    let failure_message = if csp_failure.is_some() {
        csp_failure
    } else if blocked {
        Some(BLOCKED_BY_CLIENT_ERROR_TEXT.to_owned())
    } else if network_offline {
        Some("Network emulation offline".to_owned())
    } else {
        cookie_header
            .err()
            .map(|error| format!("failed to build WebSocket cookie header: {error}"))
    };
    let request_overrideed = failure_message.is_some();
    let (connection, load) = if let Some(error_text) = failure_message {
        record_worker_websocket_subresource_failure(
            &state.borrow(),
            socket_id,
            document_url.clone(),
            url.clone(),
            context.extra_headers.clone(),
            error_text.clone(),
        );
        (
            spawn_failed_connection(socket_id, error_text, websocket_event_tx),
            None,
        )
    } else {
        let load = loader.register_load(
            ResourceLoadKind::WebSocket,
            ResourceLoadDisposition::Ordinary,
            None,
        )?;
        let connection = spawn_connection(
            loader.request_client().websocket_connector(),
            socket_id,
            url.to_string(),
            protocols,
            context,
            websocket_event_tx,
        );
        let cancel_tx = connection.clone();
        load.attach_consumer_cancel(move || {
            cancel_tx.cancel();
        });
        (connection, Some(load))
    };

    state.borrow_mut().websockets.insert(
        socket_id,
        WorkerWebSocketState {
            wrapper: v8::Global::new(scope, wrapper),
            connection,
            document_url,
            url,
            loader,
            load,
            opened: false,
            request_overrideed,
        },
    );
    Some(socket_id)
}

pub(crate) fn send_worker_websocket_text(
    scope: &mut v8::PinScope<'_, '_>,
    socket_id: u64,
    text: String,
) -> Option<bool> {
    let state = get_worker_state(scope)?;
    let connection = state
        .borrow()
        .websockets
        .get(&socket_id)?
        .connection
        .clone();
    Some(connection.send_text(text).is_ok())
}

pub(crate) fn send_worker_websocket_binary(
    scope: &mut v8::PinScope<'_, '_>,
    socket_id: u64,
    bytes: Vec<u8>,
) -> Option<bool> {
    let state = get_worker_state(scope)?;
    let connection = state
        .borrow()
        .websockets
        .get(&socket_id)?
        .connection
        .clone();
    Some(connection.send_binary(bytes).is_ok())
}

pub(crate) fn close_worker_websocket(
    scope: &mut v8::PinScope<'_, '_>,
    socket_id: u64,
    code: Option<u16>,
    reason: String,
) -> Option<bool> {
    let state = get_worker_state(scope)?;
    let connection = state
        .borrow()
        .websockets
        .get(&socket_id)?
        .connection
        .clone();
    Some(connection.close(code, reason).is_ok())
}

pub(in crate::worker) fn dispatch_worker_websocket_event(
    scope: &mut v8::PinScope<'_, '_>,
    state: &Rc<RefCell<WorkerGlobalState>>,
    event: WebSocketEvent,
) -> bool {
    let socket_id = match &event {
        WebSocketEvent::HandshakeResponse { socket_id, .. }
        | WebSocketEvent::Open { socket_id, .. }
        | WebSocketEvent::TextMessage { socket_id, .. }
        | WebSocketEvent::BinaryMessage { socket_id, .. }
        | WebSocketEvent::SendCompleted { socket_id, .. }
        | WebSocketEvent::Error { socket_id, .. }
        | WebSocketEvent::Closing { socket_id }
        | WebSocketEvent::Close { socket_id, .. } => *socket_id,
    };
    let socket_context = {
        let mut state = state.borrow_mut();
        let parent_tx = state.parent_tx.clone();
        let socket_context = state.websockets.get_mut(&socket_id).map(|entry| {
            let was_opened = entry.opened;
            let mut should_record_error_failure = false;
            match &event {
                WebSocketEvent::Open { .. } => {
                    entry.opened = true;
                    entry.request_overrideed = true;
                }
                WebSocketEvent::Error { .. } if !entry.opened && !entry.request_overrideed => {
                    entry.request_overrideed = true;
                    should_record_error_failure = true;
                }
                _ => {}
            }
            (
                v8::Local::new(scope, &entry.wrapper),
                entry.document_url.clone(),
                entry.url.clone(),
                entry.loader.clone(),
                was_opened,
                should_record_error_failure,
                parent_tx,
            )
        });
        if socket_context.is_none() {
            state.websockets.remove(&socket_id);
        }
        socket_context
    };
    let Some((
        socket,
        document_url,
        socket_url,
        loader,
        was_opened,
        should_record_error_failure,
        parent_tx,
    )) = socket_context
    else {
        return false;
    };

    let mut parent_messages = Vec::new();
    match &event {
        WebSocketEvent::Open {
            request_headers,
            response_status,
            response_headers,
            ..
        } => {
            let cookie_set_reports = store_worker_websocket_response_cookies(
                socket_id,
                &socket_url,
                &loader,
                response_headers,
            );
            parent_messages.push(WorkerToParentMessage::WebSocketLifecycle(
                WorkerWebSocketLifecycleEvent::Open {
                    socket_id,
                    document_url: document_url.clone(),
                    url: socket_url.clone(),
                },
            ));
            parent_messages.push(WorkerToParentMessage::WebSocketSubresource(
                SubresourceNetworkRecord::success(
                    None,
                    document_url.clone(),
                    socket_url.clone(),
                    "GET".to_owned(),
                    request_headers.clone(),
                    None,
                    SubresourceResourceType::WebSocket,
                    None,
                    Vec::new(),
                    socket_url.clone(),
                    *response_status,
                    response_headers.clone(),
                    String::new(),
                    cookie_set_reports,
                )
                .with_websocket_socket_id(socket_id),
            ));
        }
        WebSocketEvent::SendCompleted {
            opcode,
            payload_length,
            ..
        } => {
            parent_messages.push(WorkerToParentMessage::WebSocketFrame(
                WorkerWebSocketFrameEvent {
                    socket_id,
                    document_url: document_url.clone(),
                    url: socket_url.clone(),
                    direction: WebSocketFrameDirection::Sent,
                    opcode: worker_websocket_frame_opcode(*opcode),
                    payload_length: *payload_length,
                },
            ));
        }
        WebSocketEvent::Error { message, .. } => {
            parent_messages.push(WorkerToParentMessage::WebSocketLifecycle(
                WorkerWebSocketLifecycleEvent::Error {
                    socket_id,
                    document_url: document_url.clone(),
                    url: socket_url.clone(),
                    error_text: message.clone(),
                },
            ));
            if !was_opened && should_record_error_failure {
                parent_messages.push(WorkerToParentMessage::WebSocketSubresource(
                    SubresourceNetworkRecord::failure(
                        None,
                        document_url.clone(),
                        socket_url.clone(),
                        "GET".to_owned(),
                        Vec::new(),
                        None,
                        SubresourceResourceType::WebSocket,
                        message.clone(),
                    )
                    .with_websocket_socket_id(socket_id),
                ));
            }
        }
        WebSocketEvent::Closing { .. } => {
            parent_messages.push(WorkerToParentMessage::WebSocketLifecycle(
                WorkerWebSocketLifecycleEvent::Closing {
                    socket_id,
                    document_url: document_url.clone(),
                    url: socket_url.clone(),
                },
            ));
        }
        WebSocketEvent::Close {
            code,
            reason,
            was_clean,
            ..
        } => {
            parent_messages.push(WorkerToParentMessage::WebSocketLifecycle(
                WorkerWebSocketLifecycleEvent::Close {
                    socket_id,
                    document_url: document_url.clone(),
                    url: socket_url.clone(),
                    code: *code,
                    reason: reason.clone(),
                    was_clean: *was_clean,
                },
            ));
        }
        WebSocketEvent::HandshakeResponse { .. }
        | WebSocketEvent::TextMessage { .. }
        | WebSocketEvent::BinaryMessage { .. } => {}
    }
    for message in parent_messages {
        let _ = parent_tx.send(message);
    }

    let dispatch_result = crate::context_bootstrap::dispatch_websocket_event(scope, socket, &event);
    if !matches!(
        dispatch_result,
        crate::context_bootstrap::WebSocketDispatchResult::Backpressured
    ) {
        let frame_event = match &event {
            WebSocketEvent::TextMessage { data, .. } => Some(WorkerWebSocketFrameEvent {
                socket_id,
                document_url: document_url.clone(),
                url: socket_url.clone(),
                direction: WebSocketFrameDirection::Received,
                opcode: WebSocketFrameOpcode::Text,
                payload_length: data.len(),
            }),
            WebSocketEvent::BinaryMessage { data, .. } => Some(WorkerWebSocketFrameEvent {
                socket_id,
                document_url,
                url: socket_url,
                direction: WebSocketFrameDirection::Received,
                opcode: WebSocketFrameOpcode::Binary,
                payload_length: data.len(),
            }),
            _ => None,
        };
        if let Some(frame_event) = frame_event {
            let _ = parent_tx.send(WorkerToParentMessage::WebSocketFrame(frame_event));
        }
    }
    if matches!(event, WebSocketEvent::Close { .. })
        && let Some(socket) = state.borrow_mut().websockets.remove(&socket_id)
        && let Some(load) = socket.load
    {
        load.finish();
    }
    dispatch_result.dispatched()
}

pub(in crate::worker) fn worker_websocket_frame_opcode(
    opcode: moli_websocket::FrameOpcode,
) -> WebSocketFrameOpcode {
    match opcode {
        moli_websocket::FrameOpcode::Text => WebSocketFrameOpcode::Text,
        moli_websocket::FrameOpcode::Binary => WebSocketFrameOpcode::Binary,
    }
}

pub(in crate::worker) fn store_worker_websocket_response_cookies(
    _socket_id: u64,
    socket_url: &Url,
    request_client: &crate::network::context::WorkerResourceLoader,
    response_headers: &[(String, String)],
) -> Vec<StoredCookieSetReport> {
    if !response_headers
        .iter()
        .any(|(name, _)| response_header_name_is(name, &HeaderName::from_static("set-cookie")))
    {
        return Vec::new();
    }
    let response_cookie_url = websocket_cookie_url(socket_url);
    let cookie_store = request_client.request_client().cookie_store();
    let mut store = cookie_store.lock();
    store.store_response_headers_with_reports(&response_cookie_url, response_headers)
}

pub(in crate::worker) fn response_header_name_is(candidate: &str, expected: &HeaderName) -> bool {
    HeaderName::from_bytes(candidate.as_bytes()).is_ok_and(|candidate| candidate == *expected)
}

pub(in crate::worker) fn request_body_text(body: &Option<Vec<u8>>) -> Option<String> {
    body.as_ref()
        .map(|body| String::from_utf8_lossy(body).into_owned())
}

pub(in crate::worker) fn make_rejected_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: &str,
) -> v8::Local<'s, v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).expect("worker fetch resolver");
    let promise = resolver.get_promise(scope);
    if let Some(message) = v8_string(scope, message) {
        resolver.reject(scope, v8::Exception::type_error(scope, message));
    } else {
        resolver.reject(scope, v8::undefined(scope).into());
    }
    promise
}

pub(in crate::worker) fn make_rejected_promise_with_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    reason: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).expect("worker fetch resolver");
    let promise = resolver.get_promise(scope);
    let _ = resolver.reject(scope, reason);
    promise
}

pub(in crate::worker) fn validate_worker_fetch_signal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Result<Option<v8::Local<'s, v8::Object>>, String> {
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    let Ok(signal) = v8::Local::<v8::Object>::try_from(value) else {
        return Err(
            "Failed to execute 'fetch' on 'DedicatedWorkerGlobalScope': signal must be an AbortSignal."
                .to_owned(),
        );
    };
    if worker_abort_signal_id(scope, signal).is_none() {
        return Err(
            "Failed to execute 'fetch' on 'DedicatedWorkerGlobalScope': signal must be an AbortSignal."
                .to_owned(),
        );
    }
    Ok(Some(signal))
}

pub(in crate::worker) fn worker_fetch_signal_option<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: &v8::FunctionCallbackArguments<'s>,
    request_like: Option<v8::Local<'s, v8::Object>>,
) -> Result<Option<v8::Local<'s, v8::Object>>, String> {
    let signal_key = v8str(scope, "signal");
    if args.length() > 1 {
        let init_arg = args.get(1);
        if !init_arg.is_null_or_undefined()
            && let Ok(init) = v8::Local::<v8::Object>::try_from(init_arg)
            && init.has(scope, signal_key.into()).unwrap_or(false)
        {
            let signal = init
                .get(scope, signal_key.into())
                .unwrap_or_else(|| v8::undefined(scope).into());
            return validate_worker_fetch_signal(scope, signal);
        }
    }
    if let Some(request_like) = request_like
        && let Some(signal) = request_like.get(scope, signal_key.into())
    {
        return validate_worker_fetch_signal(scope, signal);
    }
    Ok(None)
}

pub(in crate::worker) struct ResolvedWorkerFetchInput<'s> {
    resolved_url: Url,
    method: String,
    body: Option<Vec<u8>>,
    headers: Vec<(String, String)>,
    suppress_default_content_type: bool,
    request_mode: moli_fetch::RequestMode,
    credentials_mode: RequestCredentialsMode,
    redirect_mode: RequestRedirectMode,
    priority: Option<moli_fetch::FetchPriorityHint>,
    metadata: ServiceWorkerFetchRequestMetadata,
    signal: Option<v8::Local<'s, v8::Object>>,
}

pub(in crate::worker) fn resolve_worker_fetch_input<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: &v8::FunctionCallbackArguments<'s>,
    base_url: &Url,
) -> Result<ResolvedWorkerFetchInput<'s>, String> {
    if args.length() < 1 {
        return Err(
            webidl::WebIdlError::missing_required(webidl::Context::argument("fetch", 1))
                .to_string(),
        );
    }
    let arg0 = args.get(0);
    let mut request_like = None;
    let mut consumes_request_body = false;
    let inherited = request_input_snapshot(scope, arg0).map_err(|error| error.to_string())?;
    let (
        url_input,
        method,
        body,
        headers,
        suppress_default_content_type,
        request_mode,
        credentials_mode,
        redirect_mode,
        priority,
        metadata,
    ) = if let Some(inherited) = inherited {
        let req_obj = v8::Local::<v8::Object>::try_from(arg0).expect("request-like object");
        request_like = Some(req_obj);
        let url = inherited.url.clone();
        let init = parse_fetch_init(scope, args, 1)?;
        consumes_request_body = !init.body_present && inherited.body.is_some();
        let method = if init.method_present {
            init.method.clone()
        } else {
            inherited.method.clone()
        };
        let body = if init.body_present {
            init.body.clone()
        } else {
            inherited.body.clone()
        };
        let mut headers = if init.headers_present {
            init.headers.clone()
        } else {
            inherited.headers.clone()
        };
        let suppress_default_content_type = if init.body_present {
            if !init.headers_present {
                append_default_body_content_type(&mut headers, init.body_content_type.as_deref());
            }
            init.suppress_default_content_type
                || (body.is_some()
                    && init.body_content_type.is_none()
                    && !has_header(&headers, "content-type"))
        } else {
            body.is_some() && !has_header(&headers, "content-type")
        };
        let inherited_credentials = request_object_credentials_mode(scope, req_obj)?;
        let request_mode = init
            .request_mode
            .or_else(|| moli_fetch::RequestMode::from_str(&inherited.mode).ok())
            .unwrap_or(moli_fetch::RequestMode::Cors);
        validate_worker_no_cors_method(request_mode, &method)?;
        let headers = if request_mode == moli_fetch::RequestMode::NoCors {
            filter_headers_for_guard(&headers, HeadersGuard::RequestNoCors)
        } else {
            headers
        };
        let credentials_mode = init
            .credentials_mode
            .or(inherited_credentials)
            .unwrap_or(RequestCredentialsMode::SameOrigin);
        let redirect_mode = init
            .redirect_mode
            .or_else(|| crate::network_host::parse_request_redirect_mode_label(&inherited.redirect))
            .unwrap_or(RequestRedirectMode::Follow);
        let priority = init.priority.or_else(|| {
            (inherited.priority != moli_fetch::FetchPriorityHint::Auto)
                .then_some(inherited.priority)
        });
        let metadata = ServiceWorkerFetchRequestMetadata {
            cache: init.cache.unwrap_or(inherited.cache),
            referrer: init.referrer.unwrap_or(inherited.referrer),
            referrer_policy: init.referrer_policy.unwrap_or(inherited.referrer_policy),
            integrity: init.integrity.unwrap_or(inherited.integrity),
            keepalive: init.keepalive.unwrap_or(inherited.keepalive),
            ..Default::default()
        };
        (
            url,
            method,
            body,
            headers,
            suppress_default_content_type,
            request_mode,
            credentials_mode,
            redirect_mode,
            priority,
            metadata,
        )
    } else {
        let url = webidl::convert::<webidl::UsvString>(
            scope,
            arg0,
            webidl::Context::argument("fetch", 1),
        )
        .map(String::from)
        .map_err(|error| error.to_string())?;
        let init = parse_fetch_init(scope, args, 1)?;
        let request_mode = init.request_mode.unwrap_or(moli_fetch::RequestMode::Cors);
        validate_worker_no_cors_method(request_mode, &init.method)?;
        let headers = if request_mode == moli_fetch::RequestMode::NoCors {
            filter_headers_for_guard(&init.headers, HeadersGuard::RequestNoCors)
        } else {
            init.headers
        };
        let credentials_mode = init
            .credentials_mode
            .unwrap_or(RequestCredentialsMode::SameOrigin);
        let redirect_mode = init.redirect_mode.unwrap_or(RequestRedirectMode::Follow);
        let metadata = ServiceWorkerFetchRequestMetadata {
            cache: init.cache.unwrap_or_else(|| "default".to_owned()),
            referrer: init.referrer.unwrap_or_else(|| "about:client".to_owned()),
            referrer_policy: init.referrer_policy.unwrap_or_default(),
            integrity: init.integrity.unwrap_or_default(),
            keepalive: init.keepalive.unwrap_or(false),
            ..Default::default()
        };
        (
            url,
            init.method,
            init.body,
            headers,
            init.suppress_default_content_type,
            request_mode,
            credentials_mode,
            redirect_mode,
            init.priority,
            metadata,
        )
    };
    let resolved_url = resolve_context_url(base_url, &url_input, None)?;
    let signal = worker_fetch_signal_option(scope, args, request_like)?;
    if consumes_request_body && let Some(request_like) = request_like {
        crate::network_host::mark_request_input_body_used_for_fetch(scope, request_like);
    }
    Ok(ResolvedWorkerFetchInput {
        resolved_url,
        method,
        body,
        headers,
        suppress_default_content_type,
        request_mode,
        credentials_mode,
        redirect_mode,
        priority,
        metadata,
        signal,
    })
}

pub(in crate::worker) fn validate_worker_no_cors_method(
    request_mode: RequestMode,
    method: &str,
) -> Result<(), String> {
    if request_mode == RequestMode::NoCors && !moli_fetch::is_cors_safelisted_method(method) {
        return Err(format!(
            "Failed to execute 'fetch' on 'DedicatedWorkerGlobalScope': method `{method}` is unsupported in no-cors mode."
        ));
    }
    Ok(())
}

pub(in crate::worker) fn worker_fetch_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue<'s, v8::Value>,
) {
    let Some(state) = get_worker_state(scope) else {
        rv.set(make_rejected_promise(scope, "fetch: worker runtime state is unavailable").into());
        return;
    };

    let (
        loader,
        document_url,
        extra_http_headers,
        network_offline,
        blocked_url_patterns,
        fetch_subresource_interception_enabled,
        fetch_subresource_interception_resource_type,
        policy_context,
    ) = {
        let state = state.borrow();
        let Some(document_url) = state.current_script_url.clone() else {
            rv.set(make_rejected_promise(scope, "fetch: worker script url is unavailable").into());
            return;
        };
        (
            state.loader.clone(),
            document_url,
            state.extra_http_headers.clone(),
            state.network_offline,
            state.blocked_url_patterns.clone(),
            state.fetch_subresource_interception_enabled,
            state.fetch_subresource_interception_resource_type,
            state.policy_context,
        )
    };

    let ResolvedWorkerFetchInput {
        resolved_url,
        method,
        body,
        headers: request_headers,
        suppress_default_content_type,
        request_mode,
        credentials_mode,
        redirect_mode,
        priority,
        metadata: request_metadata,
        signal,
    } = match resolve_worker_fetch_input(scope, &args, &document_url) {
        Ok(resolved) => resolved,
        Err(message) => {
            rv.set(make_rejected_promise(scope, &message).into());
            return;
        }
    };
    let headers = merge_worker_request_headers(&extra_http_headers, &request_headers);
    if let Some(signal) = signal
        && worker_abort_signal_aborted(scope, signal)
    {
        let reason = worker_abort_signal_reason(scope, signal)
            .unwrap_or_else(|| worker_abort_error_value(scope));
        rv.set(make_rejected_promise_with_value(scope, reason).into());
        return;
    }

    dispatch_worker_content_security_policy_report_only_violation_for_state(
        scope,
        &state,
        &document_url,
        &resolved_url,
        crate::content_security_policy::ContentSecurityPolicyResourceKind::WorkerConnect,
    );
    let csp_violation = {
        let state_ref = state.borrow();
        worker_content_security_policy_violation(
            &state_ref,
            &document_url,
            &resolved_url,
            crate::content_security_policy::ContentSecurityPolicyResourceKind::WorkerConnect,
        )
    };
    if let Some(violation) = csp_violation {
        dispatch_worker_content_security_policy_violation_event_for_state(
            scope, &state, &violation,
        );
        let message = worker_content_security_policy_error_message(&violation, "fetch");
        record_worker_subresource_failure(
            &state.borrow(),
            document_url,
            resolved_url,
            method,
            headers,
            body,
            SubresourceResourceType::Fetch,
            message.clone(),
        );
        rv.set(make_rejected_promise(scope, &message).into());
        return;
    }

    if let Err(error) = moli_url_policy::route_fetch_url(&resolved_url) {
        let message = error.to_string();
        record_worker_subresource_failure(
            &state.borrow(),
            document_url,
            resolved_url,
            method,
            headers,
            body,
            SubresourceResourceType::Fetch,
            message.clone(),
        );
        rv.set(make_rejected_promise(scope, &message).into());
        return;
    }

    if should_request_be_blocked_due_to_bad_port(&resolved_url) {
        let message = format!("fetch: blocked bad port for `{resolved_url}`");
        record_worker_subresource_failure(
            &state.borrow(),
            document_url,
            resolved_url,
            method,
            headers,
            body,
            SubresourceResourceType::Fetch,
            message.clone(),
        );
        rv.set(make_rejected_promise(scope, &message).into());
        return;
    }

    if worker_url_blocked(&blocked_url_patterns, &resolved_url) {
        let message = BLOCKED_BY_CLIENT_ERROR_TEXT.to_owned();
        record_worker_subresource_failure(
            &state.borrow(),
            document_url,
            resolved_url,
            method,
            headers,
            body,
            SubresourceResourceType::Fetch,
            message.clone(),
        );
        rv.set(make_rejected_promise(scope, &message).into());
        return;
    }

    if fetch_subresource_interception_enabled
        && fetch_subresource_interception_resource_type.is_none_or(|expected| {
            expected.has_same_cdp_fetch_interception_type(SubresourceResourceType::Fetch)
        })
    {
        let Some(resolver) = v8::PromiseResolver::new(scope) else {
            rv.set_undefined();
            return;
        };
        let promise = resolver.get_promise(scope);
        let signal_id = signal.and_then(|signal| worker_abort_signal_id(scope, signal));
        let cancel_handle = FetchCancelHandle::new();
        let disposition = if request_metadata.keepalive {
            ResourceLoadDisposition::Keepalive
        } else {
            ResourceLoadDisposition::Ordinary
        };
        let Some(load) = loader.register_load(
            ResourceLoadKind::Fetch,
            disposition,
            Some(cancel_handle.clone()),
        ) else {
            rv.set(make_rejected_promise(scope, "fetch: worker global is shutting down").into());
            return;
        };
        let request_body = request_body_text(&body);
        let Some(network) = ResourceTransfer::for_worker(
            state.borrow().global_kind.network(),
            state.borrow().parent_tx.network_observer(),
            |network| {
                worker_request_started(
                    network,
                    &document_url,
                    &resolved_url,
                    &method,
                    &headers,
                    &body,
                    SubresourceResourceType::Fetch,
                )
                .with_keepalive(request_metadata.keepalive)
            },
        ) else {
            rv.set(make_rejected_promise(scope, "fetch: worker global is shutting down").into());
            return;
        };
        let network_request_handle = network.handle();
        let fetch_id = {
            let mut state = state.borrow_mut();
            let fetch_id = next_fetch_id(&mut state);
            state.pending_fetches.insert(
                fetch_id,
                PendingWorkerFetch {
                    resolver: v8::Global::new(scope, resolver),
                    document_url: document_url.clone(),
                    credentials_mode,
                    request_mode,
                    redirect_mode,
                    request_priority: priority,
                    request_metadata: request_metadata.clone(),
                    policy_context,
                    signal_id,
                    load: load.clone(),
                    request_url: resolved_url.clone(),
                    request_method: method.clone(),
                    request_headers: headers.clone(),
                    request_body: body.clone(),
                    response: ResourceResponseStream::new(network),
                    request_override: None,
                    paused_response: None,
                    streaming_body_source_id: None,
                },
            );
            fetch_id
        };
        let info = PendingSubresourceFetchInfo {
            internal_id: 0,
            network_request_handle: Some(network_request_handle),
            frame_id: None,
            document_url,
            url: resolved_url,
            websocket_socket_id: None,
            method,
            request_headers: headers,
            request_body_bytes: body,
            request_body,
            resource_type: SubresourceResourceType::Fetch,
            request_cookie_report: None,
        };
        publish_worker_fetch_pause(
            &state.borrow(),
            crate::runtime::WorkerFetchTarget::Fetch(fetch_id),
            network_request_handle,
            load,
            crate::runtime::RendererWorkerFetchStage::Request(Box::new(info)),
        );
        rv.set(promise.into());
        return;
    }

    if network_offline {
        let message = "Network emulation offline".to_owned();
        record_worker_subresource_failure(
            &state.borrow(),
            document_url,
            resolved_url,
            method,
            headers,
            body,
            SubresourceResourceType::Fetch,
            message.clone(),
        );
        rv.set(make_rejected_promise(scope, &message).into());
        return;
    }

    let promise = {
        let Some(resolver) = v8::PromiseResolver::new(scope) else {
            rv.set_undefined();
            return;
        };
        let promise = resolver.get_promise(scope);
        let signal_id = signal.and_then(|signal| worker_abort_signal_id(scope, signal));
        let cancel_handle = FetchCancelHandle::new();
        let disposition = if request_metadata.keepalive {
            ResourceLoadDisposition::Keepalive
        } else {
            ResourceLoadDisposition::Ordinary
        };
        let Some(load) = loader.register_load(
            ResourceLoadKind::Fetch,
            disposition,
            Some(cancel_handle.clone()),
        ) else {
            rv.set(make_rejected_promise(scope, "fetch: worker global is shutting down").into());
            return;
        };
        let request_body = body.clone();
        let Some(network) = ResourceTransfer::for_worker(
            state.borrow().global_kind.network(),
            state.borrow().parent_tx.network_observer(),
            |network| {
                worker_request_started(
                    network,
                    &document_url,
                    &resolved_url,
                    &method,
                    &headers,
                    &body,
                    SubresourceResourceType::Fetch,
                )
                .with_keepalive(request_metadata.keepalive)
            },
        ) else {
            rv.set(make_rejected_promise(scope, "fetch: worker global is shutting down").into());
            return;
        };
        let fetch_id = {
            let mut state = state.borrow_mut();
            let fetch_id = next_fetch_id(&mut state);
            state.pending_fetches.insert(
                fetch_id,
                PendingWorkerFetch {
                    resolver: v8::Global::new(scope, resolver),
                    document_url: document_url.clone(),
                    credentials_mode,
                    request_mode,
                    redirect_mode,
                    request_priority: priority,
                    request_metadata: request_metadata.clone(),
                    policy_context,
                    signal_id,
                    load: load.clone(),
                    request_url: resolved_url.clone(),
                    request_method: method.clone(),
                    request_headers: headers.clone(),
                    request_body,
                    response: ResourceResponseStream::new(network),
                    request_override: None,
                    paused_response: None,
                    streaming_body_source_id: None,
                },
            );
            fetch_id
        };
        let controller = worker_service_worker_controller(&state.borrow(), &resolved_url);
        start_worker_fetch(
            &state,
            fetch_id,
            cancel_handle,
            None,
            suppress_default_content_type,
            controller,
        );

        promise
    };
    rv.set(promise.into());
}

impl WorkerGlobalState {
    pub(crate) fn cancel_streaming_fetch(&mut self, body_source_id: NetworkBodySourceId) {
        let Some(fetch_id) = self.pending_fetches.iter().find_map(|(fetch_id, pending)| {
            (pending.streaming_body_source_id == Some(body_source_id)).then_some(*fetch_id)
        }) else {
            return;
        };
        let pending = self
            .pending_fetches
            .remove(&fetch_id)
            .expect("streaming fetch selected from the same map");
        pending.load.cancel();
        record_worker_fetch_failure(&pending, ABORTED_ERROR_TEXT.to_owned());
    }
}

pub(in crate::worker) fn reject_worker_fetches_for_signal(
    scope: &mut v8::PinScope<'_, '_>,
    signal_id: u32,
    reason: v8::Local<'_, v8::Value>,
) {
    let Some(state) = get_worker_state(scope) else {
        return;
    };
    let rejected = {
        let mut state = state.borrow_mut();
        let fetch_ids = state
            .pending_fetches
            .iter()
            .filter_map(|(fetch_id, pending)| {
                (pending.signal_id == Some(signal_id)).then_some(*fetch_id)
            })
            .collect::<Vec<_>>();
        let mut rejected = Vec::with_capacity(fetch_ids.len());
        for fetch_id in fetch_ids {
            if let Some(pending) = state.pending_fetches.remove(&fetch_id) {
                rejected.push(pending);
            }
        }
        rejected
    };
    for mut pending in rejected {
        pending.load.cancel();
        if let Some(response) = pending.paused_response.take() {
            response.discard();
        }
        record_worker_fetch_failure(&pending, ABORTED_ERROR_TEXT.to_owned());
        if let Some(body_source_id) = pending.streaming_body_source_id {
            let abort_reason = worker_abort_error_value(scope);
            error_pending_network_body_stream_with_reason(
                scope,
                body_source_id,
                ABORTED_ERROR_TEXT.to_owned(),
                abort_reason,
            );
        } else {
            let resolver = v8::Local::new(scope, &pending.resolver);
            let _ = resolver.reject(scope, reason);
        }
    }
}

pub(in crate::worker) fn worker_response_from_body(
    final_url: Url,
    status: u16,
    headers: Vec<(String, String)>,
    body: RendererSyntheticResponseBody,
) -> Response {
    body.into_fetch_response(ResponseHead {
        final_url,
        status,
        headers,
        request_cookie_report: None,
        cookie_set_reports: Vec::new(),
        redirected: false,
        redirect_chain: Vec::new(),
        from_cache: false,
        negotiated_http_version: None,
    })
}

pub(in crate::worker) fn drain_worker_fetch_completion(
    scope: &mut v8::PinScope<'_, '_>,
    state: &Rc<RefCell<WorkerGlobalState>>,
    event: WorkerFetchEvent,
) {
    match event {
        WorkerFetchEvent::TransportCompletion(delivery) => {
            let completion = state
                .borrow()
                .pending_fetches
                .get(&delivery.request_id())
                .and_then(|pending| delivery.claim(&pending.response));
            if let Some(completion) = completion {
                drain_worker_fetch_completion_result(scope, state, completion);
            }
        }
        WorkerFetchEvent::AuthRequired(response) => response.pause(scope, state),
        WorkerFetchEvent::Completion(completion) => {
            drain_worker_fetch_completion_result(scope, state, *completion)
        }
        WorkerFetchEvent::StreamingStarted(started) => {
            start_worker_streaming_fetch(scope, state, *started)
        }
        WorkerFetchEvent::StreamingChunk(chunk) => {
            enqueue_pending_network_body_chunk(scope, chunk.body_source_id, chunk.bytes)
        }
        WorkerFetchEvent::StreamingFinished(finished) => {
            finish_worker_streaming_fetch(scope, state, finished)
        }
    }
}

pub(in crate::worker) fn start_worker_streaming_fetch(
    scope: &mut v8::PinScope<'_, '_>,
    state: &Rc<RefCell<WorkerGlobalState>>,
    started: WorkerFetchStreamingStarted,
) {
    let mut reject = None;
    let csp_failure =
        worker_fetch_response_csp_error(scope, state, started.fetch_id, &started.head);
    let response_input = if let Some(message) = csp_failure {
        let resolver = {
            let mut state_ref = state.borrow_mut();
            let Some(pending) = state_ref.pending_fetches.get_mut(&started.fetch_id) else {
                return;
            };
            pending.load.cancel();
            pending.resolver.clone()
        };
        reject = Some((resolver, message));
        None
    } else {
        let mut state_ref = state.borrow_mut();
        let Some(pending) = state_ref.pending_fetches.get_mut(&started.fetch_id) else {
            return;
        };
        if let Err(message) = validate_fetch_response_security_policy(
            &pending.document_url,
            &started.head.final_url,
            &started.head.headers,
            pending.request_mode,
            pending.credentials_mode,
            pending.policy_context,
        ) {
            pending.load.cancel();
            reject = Some((pending.resolver.clone(), message));
            None
        } else {
            let mut observable_head = started.head.clone();
            observable_head.headers = filter_cors_exposed_response_headers(
                &pending.document_url,
                &observable_head.final_url,
                &observable_head.headers,
                pending.credentials_mode,
            );
            pending.streaming_body_source_id = Some(started.body_source_id);
            Some((
                pending.resolver.clone(),
                pending.document_url.clone(),
                pending.request_mode,
                observable_head,
            ))
        }
    };
    if let Some((resolver, message)) = reject {
        let pending = state.borrow_mut().pending_fetches.remove(&started.fetch_id);
        if let Some(pending) = pending {
            record_worker_fetch_failure(&pending, message.clone());
        }
        if let Some(message) = v8_string(scope, &message) {
            let resolver = v8::Local::new(scope, &resolver);
            let _ = resolver.reject(scope, v8::Exception::type_error(scope, message));
        }
        return;
    }
    if let Some((resolver, document_url, request_mode, observable_head)) = response_input {
        let response_obj = build_fetch_response_object_from_stream_for_request_mode(
            scope,
            &document_url,
            request_mode,
            observable_head,
            started.body_source_id,
        );
        let resolver = v8::Local::new(scope, &resolver);
        let _ = resolver.resolve(scope, response_obj.into());
    }
}

pub(in crate::worker) fn finish_worker_streaming_fetch(
    scope: &mut v8::PinScope<'_, '_>,
    state: &Rc<RefCell<WorkerGlobalState>>,
    finished: WorkerFetchStreamingFinished,
) {
    let WorkerFetchStreamingFinished {
        body_source_id,
        delivery,
    } = finished;
    let completion = delivery
        .completion
        .as_ref()
        .expect("unclaimed stream completion");
    let Some(pending) = state.borrow_mut().pending_fetches.remove(&completion.id) else {
        return;
    };
    pending.load.finish();
    match &completion.result {
        Ok(_) => close_pending_network_body_stream(scope, body_source_id),
        Err(error) => {
            let message = error.to_string();
            let reason = v8_string(scope, &message)
                .map(|message| v8::Exception::type_error(scope, message))
                .unwrap_or_else(|| v8::undefined(scope).into());
            error_pending_network_body_stream_with_reason(scope, body_source_id, message, reason);
        }
    }
    // The transport packet settles the same request on delivery or queue retirement.
}

fn record_worker_fetch_success(
    pending: &PendingWorkerFetch,
    response: &ResourceBodyResponse,
    network_request_headers: Option<Vec<(String, String)>>,
) {
    response.publish(
        &pending.response.network,
        pending
            .response
            .record_request_headers(network_request_headers),
    );
}

pub(in crate::worker) fn record_worker_fetch_failure(
    pending: &PendingWorkerFetch,
    error: impl Into<ResourceResponseFailure>,
) {
    publish_worker_request_failure(&pending.response, error.into());
}

pub(super) fn publish_worker_request_failure(
    resource: &ResourceResponseStream,
    error: ResourceResponseFailure,
) {
    let mut error = match error {
        ResourceResponseFailure::Request(message) => resource.failure(message),
        error => error,
    };
    if let ResourceResponseFailure::PartialBody { response, .. } = &mut error {
        let response = Arc::make_mut(response);
        response.network_request_headers =
            resource.record_request_headers(response.network_request_headers.take());
    }
    let message = match &mut error {
        ResourceResponseFailure::Request(message)
        | ResourceResponseFailure::PartialBody { message, .. } => message,
    };
    if is_cors_policy_failure_message(message) {
        *message = FAILED_ERROR_TEXT.to_owned();
    }
    resource.network.failed(&error);
}

fn worker_fetch_response_csp_error(
    scope: &mut v8::PinScope<'_, '_>,
    state: &Rc<RefCell<WorkerGlobalState>>,
    fetch_id: u32,
    head: &ResponseHead,
) -> Option<String> {
    use crate::content_security_policy::ContentSecurityPolicyRedirectStatus;
    let (document_url, request_url) = {
        let state = state.borrow();
        let pending = state.pending_fetches.get(&fetch_id)?;
        (pending.document_url.clone(), pending.request_url.clone())
    };
    let redirect_status = if head.redirect_chain.is_empty() {
        ContentSecurityPolicyRedirectStatus::NoRedirect
    } else {
        ContentSecurityPolicyRedirectStatus::FollowedRedirect
    };
    if redirect_status == ContentSecurityPolicyRedirectStatus::FollowedRedirect {
        dispatch_worker_content_security_policy_report_only_violation_for_checked_url_with_redirect_status_for_state(
            scope, state, &document_url, &head.final_url, &request_url,
            crate::content_security_policy::ContentSecurityPolicyResourceKind::WorkerConnect, redirect_status,
        );
    }
    let violation = worker_content_security_policy_violation_for_checked_url_with_redirect_status(
        &state.borrow(),
        &document_url,
        &head.final_url,
        &request_url,
        crate::content_security_policy::ContentSecurityPolicyResourceKind::WorkerConnect,
        redirect_status,
    )?;
    dispatch_worker_content_security_policy_violation_event_for_state(scope, state, &violation);
    Some(worker_content_security_policy_error_message(
        &violation, "fetch",
    ))
}

fn pause_worker_fetch_auth(
    state: &Rc<RefCell<WorkerGlobalState>>,
    fetch_id: u32,
    head: &ResponseHead,
    response: WorkerFetchPausedResponse,
    challenge: crate::protocol_types::SubresourceAuthChallenge,
) {
    let mut state = state.borrow_mut();
    let Some(pending) = state.pending_fetches.get_mut(&fetch_id) else {
        return;
    };
    let (url, method, headers, body) = worker_fetch_request_metadata(pending);
    let info = PendingSubresourceAuthInfo {
        internal_id: pending.response.network.handle().get(),
        url: url.clone(),
        method: method.to_owned(),
        request_headers: headers.to_vec(),
        request_body: request_body_text(body),
        resource_type: SubresourceResourceType::Fetch,
        request_cookie_report: head.request_cookie_report.clone(),
        network_request_headers: pending.response.record_request_headers(None),
        challenge,
        intercept_response: pending.response.intercept_response(),
    };
    pending.paused_response = Some(response);
    let handle = pending.response.network.handle();
    let load = pending.load.clone();
    publish_worker_fetch_pause(
        &state,
        crate::runtime::WorkerFetchTarget::Fetch(fetch_id),
        handle,
        load,
        crate::runtime::RendererWorkerFetchStage::Auth(Box::new(info)),
    );
}

pub(in crate::worker) fn drain_worker_fetch_completion_result(
    scope: &mut v8::PinScope<'_, '_>,
    state: &Rc<RefCell<WorkerGlobalState>>,
    completion: WorkerRequestCompletion,
) {
    let global = scope.get_current_context().global(scope);
    let _ = global;
    if let Ok(response) = &completion.result {
        let response_head = response.head();
        let csp_failure =
            worker_fetch_response_csp_error(scope, state, completion.id, &response_head);
        if let Some(message) = csp_failure {
            let Some(pending) = state.borrow_mut().pending_fetches.remove(&completion.id) else {
                return;
            };
            pending.load.finish();
            let resolver = v8::Local::new(scope, &pending.resolver);
            record_worker_fetch_failure(
                &pending,
                response.failure(message.clone(), completion.network_request_headers.clone()),
            );
            if let Some(message) = v8_string(scope, &message) {
                let _ = resolver.reject(scope, v8::Exception::type_error(scope, message));
            } else {
                let _ = resolver.reject(scope, v8::undefined(scope).into());
            }
            return;
        }
        let auth_challenge = {
            let state = state.borrow();
            let Some(pending) = state.pending_fetches.get(&completion.id) else {
                return;
            };
            (pending.response.handle_auth_requests() && matches!(response_head.status, 401 | 407))
                .then(|| extract_subresource_auth_challenge(&response_head.headers))
                .flatten()
        };
        if let Some(challenge) = auth_challenge {
            pause_worker_fetch_auth(
                state,
                completion.id,
                &response_head,
                WorkerFetchPausedResponse::Complete(Box::new(response.clone())),
                challenge,
            );
            return;
        }
        let response_paused = {
            let mut state = state.borrow_mut();
            let Some(pending) = state.pending_fetches.get_mut(&completion.id) else {
                return;
            };
            match pending.request_override.as_ref() {
                Some(record) if pending.response.intercept_response() => {
                    let response_body = response.subresource_response_body();
                    let info = PendingSubresourceResponseInfo {
                        internal_id: pending.response.network.handle().get(),
                        url: record.url.clone(),
                        final_url: response_head.final_url.clone(),
                        method: record.method.clone(),
                        request_headers: record.request_headers.clone(),
                        request_body: request_body_text(&record.request_body),
                        resource_type: SubresourceResourceType::Fetch,
                        request_cookie_report: response_head.request_cookie_report.clone(),
                        network_request_headers: pending.response.record_request_headers(None),
                        response_status: response_head.status,
                        response_headers: response_head.headers.clone(),
                        response_body: response_body.clone(),
                        from_cache: response_head.from_cache,
                    };
                    pending.paused_response = Some(WorkerFetchPausedResponse::Complete(Box::new(
                        ResourceBodyResponse {
                            head: response_head,
                            body: response_body,
                        },
                    )));
                    Some(info)
                }
                _ => None,
            }
        };
        if let Some(info) = response_paused {
            let state = state.borrow();
            let pending = &state.pending_fetches[&completion.id];
            publish_worker_fetch_pause(
                &state,
                crate::runtime::WorkerFetchTarget::Fetch(completion.id),
                pending.response.network.handle(),
                pending.load.clone(),
                crate::runtime::RendererWorkerFetchStage::Response(Box::new(info)),
            );
            return;
        }
    }
    let Some(pending) = state.borrow_mut().pending_fetches.remove(&completion.id) else {
        return;
    };
    pending.load.finish();
    let resolver = v8::Local::new(scope, &pending.resolver);
    match completion.result {
        Ok(response) => {
            let response_head = response.head();
            let security_validation = response
                .body
                .try_bytes()
                .map_err(|error| {
                    FetchResponseSecurityViolation::Rejected(format!(
                        "fetch: failed to read response body: {error}"
                    ))
                })
                .and_then(|body_bytes| {
                    validate_fetch_response_security_policy_with_body_classified(
                        &pending.document_url,
                        &response_head.final_url,
                        &response_head.headers,
                        &body_bytes,
                        pending.request_mode,
                        pending.credentials_mode,
                        pending.policy_context,
                    )
                });
            let opaque_response_blocked = match security_validation {
                Ok(()) => false,
                Err(FetchResponseSecurityViolation::OpaqueResponseBlocked(_)) => true,
                Err(violation) => {
                    let message = violation.into_message();
                    record_worker_fetch_failure(
                        &pending,
                        response
                            .failure(message.clone(), completion.network_request_headers.clone()),
                    );
                    if let Some(message) = v8_string(scope, &message) {
                        let _ = resolver.reject(scope, v8::Exception::type_error(scope, message));
                    } else {
                        let _ = resolver.reject(scope, v8::undefined(scope).into());
                    }
                    return;
                }
            };
            if opaque_response_blocked {
                record_worker_fetch_failure(
                    &pending,
                    response.failure(
                        ABORTED_ERROR_TEXT.to_owned(),
                        completion.network_request_headers.clone(),
                    ),
                );
            }
            if !opaque_response_blocked {
                record_worker_fetch_success(
                    &pending,
                    &response,
                    completion.network_request_headers,
                );
            }
            let filtered_headers = filter_cors_exposed_response_headers(
                &pending.document_url,
                &response_head.final_url,
                &response_head.headers,
                pending.credentials_mode,
            );
            let ResourceBodyResponse { mut head, body } = response;
            head.headers = filtered_headers;
            let body = if opaque_response_blocked {
                SubresourceResponseBody::from_bytes(Vec::new())
            } else {
                body
            };
            let response_obj = build_fetch_response_object_from_subresource_body_for_request_mode(
                scope,
                &pending.document_url,
                pending.request_mode,
                head,
                body,
            );
            let _ = resolver.resolve(scope, response_obj.into());
        }
        Err(message) => {
            record_worker_fetch_failure(&pending, message.clone());
            if let Some(message) = v8_string(scope, &message.to_string()) {
                let _ = resolver.reject(scope, v8::Exception::type_error(scope, message));
            } else {
                let _ = resolver.reject(scope, v8::undefined(scope).into());
            }
        }
    }
}

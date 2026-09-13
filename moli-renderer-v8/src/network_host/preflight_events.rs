use moli_fetch::{FetchCancelHandle, Request, ResponseHead};

use crate::{
    network::ResourceRequestClient,
    network::ResourceTransfer,
    page_task_queue::RendererResourceCompletionSender,
    runtime::RendererDocumentNetworkReporter,
    runtime::RendererNetworkRequest,
    types::{
        AsyncSubresourceFetchEvent, AsyncSubresourceNetworkContext, SubresourceNetworkRecord,
        SubresourceRequestInitiatorType, SubresourceResponseBody,
    },
    worker::WorkerNetworkObserver,
};

#[derive(Clone)]
pub(crate) enum CorsPreflightNetworkObserver {
    Page {
        completion_tx: RendererResourceCompletionSender,
        context: AsyncSubresourceNetworkContext,
        network_reporter: Option<Box<RendererDocumentNetworkReporter>>,
    },
    Worker {
        request: RendererNetworkRequest,
        observer: WorkerNetworkObserver,
        keepalive: bool,
    },
}

impl CorsPreflightNetworkObserver {
    pub(in crate::network_host) fn new(
        completion_tx: RendererResourceCompletionSender,
        context: AsyncSubresourceNetworkContext,
    ) -> Self {
        let network_reporter = completion_tx.network_reporter().map(Box::new);
        Self::Page {
            completion_tx,
            context,
            network_reporter,
        }
    }

    pub(in crate::network_host) async fn fetch(
        &self,
        loader: &ResourceRequestClient,
        request: Request,
        cancel: Option<FetchCancelHandle>,
    ) -> Result<ResponseHead, String> {
        match self {
            Self::Worker {
                request: parent,
                observer,
                keepalive,
            } => {
                let network =
                    ResourceTransfer::preflight(parent, observer.clone(), &request, *keepalive);
                let result = loader
                    .fetch_observed_script_text_with_cancel(
                        request,
                        cancel.unwrap_or_default(),
                        network.as_ref(),
                    )
                    .await;
                network.complete(&result);
                result
                    .map(|response| response.head())
                    .map_err(|error| error.to_string())
            }
            Self::Page {
                completion_tx,
                context,
                network_reporter,
            } => {
                let request_url = request.url.clone();
                let request_headers = request.request_headers.clone();
                if let Some(network_reporter) = network_reporter.as_deref() {
                    let Some(network_request) = network_reporter.start_request() else {
                        return Err("Document network source is closed".to_owned());
                    };
                    let transfer = ResourceTransfer::from_request(
                        network_request,
                        {
                            let completion_tx = completion_tx.clone();
                            move |observation| {
                                let _ = completion_tx.send_async_subresource_event(
                                    AsyncSubresourceFetchEvent::NativeNetwork(observation),
                                );
                            }
                        },
                        move |network| {
                            moli_page_types::SubresourceRequestStarted::new(
                                network.handle(),
                                context.frame_id.clone(),
                                context.document_url.clone(),
                                request_url.clone(),
                                "OPTIONS".to_owned(),
                                request_headers.clone(),
                                None,
                                context.resource_type,
                                SubresourceRequestInitiatorType::Other,
                                None,
                            )
                        },
                    );
                    let result = loader
                        .fetch_observed_script_text_with_cancel(
                            request,
                            cancel.unwrap_or_default(),
                            transfer.as_ref(),
                        )
                        .await;
                    transfer.complete(&result);
                    return result
                        .map(|response| response.head())
                        .map_err(|error| error.to_string());
                }

                let result =
                    super::async_fetch::fetch_response_head_once(loader, request, cancel).await;
                let record = match &result {
                    Ok(response) => SubresourceNetworkRecord::success_with_body(
                        context.frame_id.clone(),
                        context.document_url.clone(),
                        request_url,
                        "OPTIONS".to_owned(),
                        request_headers,
                        None,
                        context.resource_type,
                        response.request_cookie_report.clone(),
                        response
                            .redirect_chain
                            .clone()
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                        response.final_url.clone(),
                        response.status,
                        response.headers.clone(),
                        SubresourceResponseBody::from_bytes(Vec::new()),
                        response.cookie_set_reports.clone(),
                    )
                    .with_from_cache(response.from_cache)
                    .with_negotiated_http_version(response.negotiated_http_version),
                    Err(error) => SubresourceNetworkRecord::failure(
                        context.frame_id.clone(),
                        context.document_url.clone(),
                        request_url,
                        "OPTIONS".to_owned(),
                        request_headers,
                        None,
                        context.resource_type,
                        error.clone(),
                    ),
                };
                let _ = completion_tx.send_async_subresource_event(
                    AsyncSubresourceFetchEvent::ObservedNetworkRecord(Box::new(record)),
                );
                result
            }
        }
    }
}

use moli_fetch::RedirectInfo;
use url::Url;

use moli_page_types::{
    ScriptNetworkOutputItem, SubresourceBodyFinished, SubresourceDataReceived,
    SubresourceResourceType, SubresourceResponseBody,
};
use parking_lot::Mutex;
use std::sync::Arc;

use super::{
    WorkerNetworkObserver,
    global_scope::{
        publish_worker_network_item, record_worker_fetch_response, worker_request_started,
    },
};
use crate::{
    network::{
        ScriptResponseFailure, ScriptResponseHead, ScriptResponseObserver, ScriptResponseResult,
    },
    runtime::{RendererWorkerNetworkReporter, RendererWorkerNetworkRequest},
};

/// One script consumer's publication and completion permission. Shared cache
/// work retains this request, never its VM or parent pump. Losing a consumer
/// settles only that request; the cache owns whether transport should cancel.
pub(super) struct WorkerScriptTransfer {
    state: Mutex<WorkerScriptTransferState>,
    observer: WorkerNetworkObserver,
}

enum WorkerScriptTransferState {
    Requested(RendererWorkerNetworkRequest),
    Responding(RendererWorkerNetworkRequest),
    Finished,
}

impl WorkerScriptTransfer {
    pub(super) fn start(
        source: &RendererWorkerNetworkReporter,
        observer: WorkerNetworkObserver,
        document_url: &Url,
        url: &Url,
    ) -> Option<Arc<Self>> {
        let network = source.start_request()?;
        publish_worker_network_item(
            &observer,
            &network,
            ScriptNetworkOutputItem::SubresourceRequestStarted(Arc::new(worker_request_started(
                &network,
                document_url,
                url,
                "GET",
                &[],
                &None,
                SubresourceResourceType::Script,
            ))),
        );
        Some(Arc::new(Self {
            state: Mutex::new(WorkerScriptTransferState::Requested(network)),
            observer,
        }))
    }

    pub(super) fn complete(&self, result: &ScriptResponseResult) {
        match result {
            Ok(response) => self.response_completed(response),
            Err(error) => self.failed(error),
        }
    }

    pub(super) fn response_completed(&self, response: &moli_fetch::Response) {
        let previous =
            std::mem::replace(&mut *self.state.lock(), WorkerScriptTransferState::Finished);
        let (network, body) = match previous {
            WorkerScriptTransferState::Requested(network) => {
                record_worker_fetch_response(&self.observer, &network, response.head(), None);
                let body = SubresourceBodyFinished::ready(
                    network.handle(),
                    SubresourceResponseBody::from_fetch_response(response),
                );
                (network, body)
            }
            WorkerScriptTransferState::Responding(network) => {
                let body = SubresourceBodyFinished::ready_after_streaming(
                    network.handle(),
                    SubresourceResponseBody::from_fetch_response(response),
                );
                (network, body)
            }
            WorkerScriptTransferState::Finished => return,
        };
        publish_worker_network_item(
            &self.observer,
            &network,
            ScriptNetworkOutputItem::SubresourceBodyFinished(Arc::new(body)),
        );
    }

    pub(super) fn failed(&self, error: &ScriptResponseFailure) {
        let previous =
            std::mem::replace(&mut *self.state.lock(), WorkerScriptTransferState::Finished);
        let network = match previous {
            WorkerScriptTransferState::Requested(network) => {
                if let ScriptResponseFailure::PartialBody { response, .. } = error {
                    record_worker_fetch_response(
                        &self.observer,
                        &network,
                        response.head.clone(),
                        response.network_request_headers.clone(),
                    );
                }
                network
            }
            WorkerScriptTransferState::Responding(network) => network,
            WorkerScriptTransferState::Finished => return,
        };
        let body = match error {
            ScriptResponseFailure::Request(message) => {
                SubresourceBodyFinished::failed(network.handle(), message.clone())
            }
            ScriptResponseFailure::PartialBody { message, body, .. } => {
                SubresourceBodyFinished::failed_with_partial_body(
                    network.handle(),
                    message.clone(),
                    body.clone(),
                )
            }
        };
        publish_worker_network_item(
            &self.observer,
            &network,
            ScriptNetworkOutputItem::SubresourceBodyFinished(Arc::new(body)),
        );
    }
}

impl ScriptResponseObserver for WorkerScriptTransfer {
    fn response_started(&self, response: Arc<ScriptResponseHead>) {
        let mut state = self.state.lock();
        match std::mem::replace(&mut *state, WorkerScriptTransferState::Finished) {
            WorkerScriptTransferState::Requested(network) => {
                record_worker_fetch_response(
                    &self.observer,
                    &network,
                    response.head.clone(),
                    response.network_request_headers.clone(),
                );
                *state = WorkerScriptTransferState::Responding(network);
            }
            WorkerScriptTransferState::Responding(_) => {
                panic!("one script response head per consumer")
            }
            WorkerScriptTransferState::Finished => {}
        }
    }

    fn data_received(&self, bytes: usize) {
        let state = self.state.lock();
        match &*state {
            WorkerScriptTransferState::Responding(network) => publish_worker_network_item(
                &self.observer,
                network,
                ScriptNetworkOutputItem::SubresourceDataReceived(SubresourceDataReceived::new(
                    network.handle(),
                    bytes,
                    bytes,
                )),
            ),
            WorkerScriptTransferState::Requested(_) => {
                panic!("script data must follow its response head")
            }
            WorkerScriptTransferState::Finished => {}
        }
    }
}

impl Drop for WorkerScriptTransfer {
    fn drop(&mut self) {
        if !matches!(self.state.get_mut(), WorkerScriptTransferState::Finished) {
            self.failed(&ScriptResponseFailure::Request(
                "Worker script load cancelled".into(),
            ));
        }
    }
}

pub(crate) fn ensure_worker_script_redirect_chain_same_origin(
    initiator_url: &Url,
    redirect_chain: &[RedirectInfo],
    final_url: &Url,
) -> Result<(), String> {
    if !matches!(initiator_url.scheme(), "http" | "https") {
        return Ok(());
    }
    for redirect in redirect_chain {
        if !moli_url::same_origin(initiator_url, &redirect.from_url) {
            return Err(format!(
                "cross-origin redirect from `{}` is not allowed.",
                redirect.from_url
            ));
        }
        if !moli_url::same_origin(initiator_url, &redirect.to_url) {
            return Err(format!(
                "cross-origin redirect to `{}` is not allowed.",
                redirect.to_url
            ));
        }
    }
    if !moli_url::same_origin(initiator_url, final_url) {
        return Err(format!(
            "cross-origin redirect to `{final_url}` is not allowed."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_script_terminal_is_single_use_with_or_without_streaming() {
        use crate::runtime::RendererNetworkOutputItem;
        use crate::worker::WorkerToParentMessage;
        use moli_page_types::SubresourceBodyFinishedResult;

        for streamed in [false, true] {
            for failed in [false, true] {
                let source = RendererWorkerNetworkReporter::unobserved_for_test();
                let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
                let url = url("data:text/javascript,//ok");
                let response = crate::network_host::local_url_response(&url).unwrap();
                let head = Arc::new(ScriptResponseHead {
                    head: response.head(),
                    network_request_headers: None,
                });
                let transfer = WorkerScriptTransfer::start(
                    &source,
                    WorkerNetworkObserver::Channel(send.downgrade()),
                    &url,
                    &url,
                )
                .unwrap();
                if streamed {
                    transfer.response_started(head.clone());
                    transfer.data_received(2);
                    if !failed {
                        transfer.data_received(2);
                    }
                }
                if failed {
                    transfer.failed(&ScriptResponseFailure::PartialBody {
                        message: "truncated".into(),
                        response: head.clone(),
                        body: SubresourceResponseBody::from_bytes(b"//".to_vec()),
                    });
                } else {
                    transfer.response_completed(&response);
                }
                // Late callbacks and the final lease drop cannot duplicate or
                // change the committed terminal result.
                transfer.failed(&ScriptResponseFailure::Request("late".into()));
                transfer.response_completed(&response);
                transfer.response_started(head);
                transfer.data_received(2);
                drop(transfer);
                let mut items = Vec::new();
                while let Ok(WorkerToParentMessage::Network(observation)) = receive.try_recv() {
                    let RendererNetworkOutputItem::Resource(item) = observation.item() else {
                        panic!("script producer must publish resource facts")
                    };
                    items.push(item.clone());
                }
                assert_eq!(
                    items.len(),
                    if streamed {
                        if failed { 4 } else { 5 }
                    } else {
                        3
                    }
                );
                let ScriptNetworkOutputItem::SubresourceRequestStarted(start) = items[0].as_ref()
                else {
                    panic!("start first")
                };
                let ScriptNetworkOutputItem::SubresourceResponseStarted(head) = items[1].as_ref()
                else {
                    panic!("one real response head before terminal")
                };
                assert_eq!(head.handle(), start.handle());
                let ScriptNetworkOutputItem::SubresourceBodyFinished(terminal) =
                    items.last().unwrap().as_ref()
                else {
                    panic!("terminal last")
                };
                assert_eq!(terminal.handle(), start.handle());
                match (failed, terminal.result()) {
                    (false, SubresourceBodyFinishedResult::Ready(body)) => {
                        assert_eq!(body.clone_body_bytes(), b"//ok");
                        assert_eq!(terminal.data_was_streamed(), streamed);
                    }
                    (
                        true,
                        SubresourceBodyFinishedResult::FailedWithPartialBody {
                            error_text,
                            partial_body,
                        },
                    ) => {
                        assert_eq!(error_text, "truncated");
                        assert_eq!(partial_body.clone_body_bytes(), b"//");
                    }
                    result => panic!("the first terminal outcome must win: {result:?}"),
                }
            }
        }
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn redirect(from_url: &Url, to_url: &Url) -> RedirectInfo {
        RedirectInfo {
            from_url: from_url.clone(),
            to_url: to_url.clone(),
            status: 302,
            headers: Vec::new(),
            network_extra_info_available: true,
            request_extra_info: None,
            response_extra_info: None,
            redirect_has_extra_info: true,
            request_cookie_report: None,
            cookie_set_reports: Vec::new(),
            from_cache: false,
            negotiated_http_version: None,
        }
    }

    #[test]
    fn worker_script_redirect_chain_rejects_cross_origin_final_url() {
        let initiator = url("https://app.test/page.html");
        let same_origin = url("https://app.test/worker.js");
        let cross_origin = url("https://evil.test/worker.js");

        assert!(
            ensure_worker_script_redirect_chain_same_origin(&initiator, &[], &same_origin).is_ok()
        );
        assert!(
            ensure_worker_script_redirect_chain_same_origin(&initiator, &[], &cross_origin)
                .is_err()
        );
    }

    #[test]
    fn worker_script_redirect_chain_rejects_cross_origin_intermediate_hop() {
        let initiator = url("https://app.test/page.html");
        let same_origin_redirect = url("https://app.test/redirect");
        let cross_origin_redirect = url("https://evil.test/redirect");
        let same_origin_final = url("https://app.test/worker.js");
        let chain = vec![
            redirect(&same_origin_redirect, &cross_origin_redirect),
            redirect(&cross_origin_redirect, &same_origin_final),
        ];

        assert!(
            ensure_worker_script_redirect_chain_same_origin(&initiator, &chain, &same_origin_final)
                .is_err()
        );
    }

    #[test]
    fn worker_script_redirect_chain_accepts_same_origin_hops() {
        let initiator = url("https://app.test/page.html");
        let same_origin_redirect = url("https://app.test/redirect");
        let same_origin_middle = url("https://app.test/middle");
        let same_origin_final = url("https://app.test/worker.js");
        let chain = vec![
            redirect(&same_origin_redirect, &same_origin_middle),
            redirect(&same_origin_middle, &same_origin_final),
        ];

        assert!(
            ensure_worker_script_redirect_chain_same_origin(&initiator, &chain, &same_origin_final)
                .is_ok()
        );
    }

    #[test]
    fn worker_script_redirect_chain_skips_opaque_initiators() {
        let initiator = url("data:text/html,hello");
        let cross_origin = url("https://evil.test/worker.js");

        assert!(
            ensure_worker_script_redirect_chain_same_origin(&initiator, &[], &cross_origin).is_ok()
        );
    }
}

use super::*;
use moli_fetch::StreamingRawResponse;

/// A resource result returns to the Worker that admitted it. The response and
/// load outlive the VM when its completion queue closes during transport.
pub(crate) struct WorkerFetchCompletionSender {
    pub(crate) response: Arc<ResourceResponseStream>,
    load: ResourceLoadLease,
    fetch_id: u32,
    sender: Option<mpsc::UnboundedSender<WorkerFetchEvent>>,
    body_source_id: Option<NetworkBodySourceId>,
    preflight: crate::network_host::CorsPreflightNetworkObserver,
}

/// A granted stream can enqueue body messages without retaining completion authority.
#[derive(Clone)]
pub(crate) struct WorkerFetchStreamSender {
    fetch_id: u32,
    sender: mpsc::UnboundedSender<WorkerFetchEvent>,
    body_source_id: NetworkBodySourceId,
}

impl WorkerFetchCompletionSender {
    pub(super) fn new(
        state: &WorkerGlobalState,
        pending: &PendingWorkerFetch,
        fetch_id: u32,
    ) -> Self {
        let observer = state.parent_tx.network_observer();
        Self {
            response: pending.response.clone(),
            load: pending.load.clone(),
            fetch_id,
            sender: Some(state.fetch_completion_tx.clone()),
            body_source_id: None,
            preflight: crate::network_host::CorsPreflightNetworkObserver {
                request: pending.response.network.request(),
                observer: Arc::new(move |event| observer.publish(event)),
                frame_id: None,
                resource_type: SubresourceResourceType::Fetch,
                keepalive: pending.request_metadata.keepalive,
            },
        }
    }

    pub(crate) fn stream_sender(
        &mut self,
        body_source_id: NetworkBodySourceId,
    ) -> WorkerFetchStreamSender {
        self.body_source_id = Some(body_source_id);
        WorkerFetchStreamSender {
            fetch_id: self.fetch_id,
            sender: self
                .sender
                .as_ref()
                .expect("active Worker response producer")
                .clone(),
            body_source_id,
        }
    }

    pub(crate) fn complete(
        mut self,
        result: Result<ResourceBodyResponse, ResourceResponseFailure>,
        network_request_headers: Option<Vec<(String, String)>>,
    ) {
        self.send_completion(result, network_request_headers);
    }

    fn send_completion(
        &mut self,
        result: Result<ResourceBodyResponse, ResourceResponseFailure>,
        network_request_headers: Option<Vec<(String, String)>>,
    ) {
        let Some(sender) = self.sender.take() else {
            return;
        };
        if !result
            .as_ref()
            .is_ok_and(|response| self.response.intercepts_response(&response.head))
        {
            self.load.finish();
        }
        let delivery = WorkerRequestDelivery::new(
            self.response.clone(),
            WorkerRequestCompletion {
                id: self.fetch_id,
                network_request_headers,
                result,
            },
        );
        let event = match self.body_source_id {
            Some(body_source_id) => {
                WorkerFetchEvent::StreamingFinished(WorkerFetchStreamingFinished {
                    body_source_id,
                    delivery,
                })
            }
            None => WorkerFetchEvent::TransportCompletion(delivery),
        };
        let _ = sender.send(event);
    }

    pub(crate) fn fetch_network(
        self,
        mut request: Request,
        cancel: FetchCancelHandle,
        preflight_headers: Vec<(String, String)>,
        redirects: Vec<moli_fetch::RedirectInfo>,
    ) {
        // Synthetic redirects change the wire request while retaining the
        // originating fetch's cache, referrer, integrity and partition policy.
        for redirect in &redirects {
            request.apply_redirect_status(redirect.status);
            request.url = redirect.to_url.clone();
        }
        self.load.task_runner().spawn(async move {
            let result = if matches!(request.url.scheme(), "data" | "blob") {
                local_url_response(&request.url)
                    .map(ResourceBodyResponse::from)
                    .ok_or_else(|| {
                        ResourceResponseFailure::Request(format!(
                            "fetch: local url `{}` is unavailable",
                            request.url
                        ))
                    })
            } else {
                let stream_to_script =
                    request.request_mode != RequestMode::NoCors && request.follow_redirects;
                let loader = self.load.request_client();
                match fetch_browser_subresource_raw_stream_with_preflight_headers_and_observer(
                    &loader,
                    request,
                    Some(cancel),
                    preflight_headers,
                    Some(&self.preflight),
                )
                .await
                {
                    Ok(observed) => {
                        let (mut response, headers) = worker_network_result_parts(observed);
                        self.response.record_request_headers(headers);
                        if !redirects.is_empty() {
                            let mut chain = redirects;
                            chain.append(&mut response.redirect_chain);
                            response.redirect_chain = chain;
                            response.redirected = true;
                        }
                        if self.response.handle_auth_requests()
                            && matches!(response.status, 401 | 407)
                            && extract_subresource_auth_challenge(&response.headers).is_some()
                        {
                            self.response.response_started(ResourceResponseHead {
                                status_text: None,
                                head: response.head(),
                                network_request_headers: None,
                            });
                            let sender = self.sender.as_ref().expect("active producer").clone();
                            let _ = sender.send(WorkerFetchEvent::AuthRequired(Box::new(
                                WorkerFetchAuthResponse {
                                    transfer: Some((self, response, stream_to_script)),
                                },
                            )));
                        } else {
                            self.receive_response(response, stream_to_script).await;
                        }
                        return;
                    }
                    Err(error) => {
                        let message = format!("fetch: {error}");
                        Err(error.with_message(message))
                    }
                }
            };
            self.complete(result, None);
        });
    }

    async fn receive_response(
        mut self,
        mut response: StreamingRawResponse,
        stream_to_script: bool,
    ) {
        let head = response.head();
        self.response.response_started(ResourceResponseHead {
            status_text: None,
            head: head.clone(),
            network_request_headers: None,
        });
        let stream = if stream_to_script && !self.response.intercepts_response(&head) {
            let stream = self.stream_sender(crate::network_host::new_network_body_source_id());
            stream.streaming_started(head);
            Some(stream)
        } else {
            None
        };
        while let Some(bytes) = response.next_chunk().await {
            self.response.data_received(&bytes);
            if let Some(stream) = &stream {
                stream.streaming_chunk(bytes);
            }
        }
        let result = match response.finish().await {
            Ok(()) => Ok(self.response.finish_response().expect("received response")),
            Err(error) => Err(self.response.failure(format!("fetch: {error}"))),
        };
        self.complete(result, None);
    }

    fn discard_response(&self, response: &mut StreamingRawResponse) {
        response.cancellation_handle().cancel();
        while let Some(bytes) = response.try_next_chunk() {
            self.response.data_received(&bytes);
        }
    }
}

/// Authentication retains the actual response and its original completion
/// authority. A retry discards that transport before starting the next one.
pub(in crate::worker) struct WorkerFetchAuthResponse {
    transfer: Option<(WorkerFetchCompletionSender, StreamingRawResponse, bool)>,
}

pub(in crate::worker) enum WorkerFetchPausedResponse {
    Complete(Box<ResourceBodyResponse>),
    Auth(Box<WorkerFetchAuthResponse>),
}

impl WorkerFetchPausedResponse {
    pub(in crate::worker) fn discard(self) -> ResponseHead {
        match self {
            Self::Complete(response) => response.head,
            Self::Auth(mut response) => {
                let (mut producer, mut response, _) =
                    response.transfer.take().expect("held response");
                producer.discard_response(&mut response);
                producer.sender.take();
                response.head()
            }
        }
    }

    pub(in crate::worker) fn resume(
        self,
        sender: &mpsc::UnboundedSender<WorkerFetchEvent>,
        fetch_id: u32,
        response_code: Option<u16>,
        response_headers: Option<Vec<(String, String)>>,
    ) {
        match self {
            Self::Complete(mut response) => {
                if let Some(status) = response_code {
                    response.head.status = status;
                }
                if let Some(headers) = response_headers {
                    response.head.headers = headers;
                }
                let _ = sender.send(WorkerFetchEvent::Completion(Box::new(
                    WorkerRequestCompletion {
                        id: fetch_id,
                        network_request_headers: None,
                        result: Ok(*response),
                    },
                )));
            }
            Self::Auth(mut response) => {
                let (producer, mut response, stream) =
                    response.transfer.take().expect("held response");
                if let Some(status) = response_code {
                    response.status = status;
                }
                if let Some(headers) = response_headers {
                    response.headers = headers;
                }
                producer
                    .load
                    .task_runner()
                    .spawn(producer.receive_response(response, stream));
            }
        }
    }
}

impl WorkerFetchAuthResponse {
    pub(super) fn pause(
        self: Box<Self>,
        scope: &mut v8::PinScope<'_, '_>,
        state: &Rc<RefCell<WorkerGlobalState>>,
    ) {
        let (producer, response, _) = self.transfer.as_ref().expect("held response");
        let fetch_id = producer.fetch_id;
        if !state
            .borrow()
            .pending_fetches
            .get(&fetch_id)
            .is_some_and(|pending| {
                Arc::ptr_eq(&pending.response, &producer.response) && !pending.load.is_cancelled()
            })
        {
            return;
        }
        let head = response.head();
        if let Some(message) = worker_fetch_response_csp_error(scope, state, fetch_id, &head) {
            let mut response = self;
            let (producer, mut response, _) = response.transfer.take().expect("held response");
            producer.discard_response(&mut response);
            drop(response);
            let failure = producer.response.failure(message);
            producer.complete(Err(failure), None);
            return;
        }
        let challenge =
            extract_subresource_auth_challenge(&head.headers).expect("authentication challenge");
        pause_worker_fetch_auth(
            state,
            fetch_id,
            &head,
            WorkerFetchPausedResponse::Auth(self),
            challenge,
        );
    }
}

impl Drop for WorkerFetchAuthResponse {
    fn drop(&mut self) {
        if let Some((producer, mut response, _)) = self.transfer.take() {
            if producer.load.is_cancelled() {
                producer.discard_response(&mut response);
                let failure = producer.response.failure(ABORTED_ERROR_TEXT.into());
                producer.complete(Err(failure), None);
                return;
            }
            // Lost observers release the decision. The existing load lease has
            // already cancelled ordinary retired requests; detached keepalive
            // responses finish without retaining or re-entering the Worker VM.
            producer.response.configure_interception(false, false);
            producer
                .load
                .task_runner()
                .spawn(producer.receive_response(response, false));
        }
    }
}

impl WorkerFetchStreamSender {
    pub(crate) fn streaming_started(&self, head: ResponseHead) {
        let body_source_id = self.body_source_id;
        let _ = self
            .sender
            .send(WorkerFetchEvent::StreamingStarted(Box::new(
                WorkerFetchStreamingStarted {
                    fetch_id: self.fetch_id,
                    body_source_id,
                    head,
                },
            )));
    }

    pub(crate) fn streaming_chunk(&self, bytes: Vec<u8>) {
        let body_source_id = self.body_source_id;
        let _ = self.sender.send(WorkerFetchEvent::StreamingChunk(
            WorkerFetchStreamingChunk {
                body_source_id,
                bytes,
            },
        ));
    }
}

impl Drop for WorkerFetchCompletionSender {
    fn drop(&mut self) {
        if self.sender.is_some() {
            self.load.cancel();
            let failure = self
                .response
                .failure("Worker response producer closed".into());
            self.send_completion(Err(failure), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropped_worker_response_producer_settles_its_original_route() {
        for streamed in [false, true] {
            let cancel = FetchCancelHandle::new();
            let (mut producer, mut receive) = producer_for_test(cancel.clone());
            let response = producer.response.clone();
            let stream = streamed
                .then(|| producer.stream_sender(crate::network_host::new_network_body_source_id()));
            drop(producer);
            assert!(
                cancel.is_cancelled(),
                "a vanished producer must cancel its transport"
            );
            let delivery = match receive.recv().await.unwrap() {
                WorkerFetchEvent::TransportCompletion(delivery) if !streamed => delivery,
                WorkerFetchEvent::StreamingFinished(finished) if streamed => {
                    assert_eq!(
                        finished.body_source_id,
                        stream.as_ref().unwrap().body_source_id
                    );
                    finished.delivery
                }
                _ => panic!("the original response route must receive its failure"),
            };
            let completion = delivery
                .claim(&response)
                .expect("the originating response must own the completion");
            assert_eq!(completion.id, 42);
            assert!(
                matches!(completion.result, Err(ResourceResponseFailure::Request(message)) if message == "Worker response producer closed")
            );
            // A view of the JS stream may outlive the producer but cannot retain
            // its load or prevent delivery of the terminal result.
            drop(stream);
            assert!(
                receive.recv().await.is_none(),
                "only one completion is sent"
            );
        }
    }

    fn producer_for_test(
        cancel: FetchCancelHandle,
    ) -> (
        WorkerFetchCompletionSender,
        mpsc::UnboundedReceiver<WorkerFetchEvent>,
    ) {
        let client =
            crate::network::ResourceRequestClient::new(&moli_fetch::FetchConfig::default())
                .unwrap();
        let load = crate::network::loads::resource_load_lease_for_test(
            client.handle(),
            Some(cancel.clone()),
        );
        let response = ResourceResponseStream::unobserved_for_test();
        let (send, receive) = mpsc::unbounded_channel();
        let producer = WorkerFetchCompletionSender {
            response: response.clone(),
            load,
            fetch_id: 42,
            sender: Some(send),
            body_source_id: None,
            preflight: crate::network_host::CorsPreflightNetworkObserver {
                request: response.network.request(),
                observer: Arc::new(|_| {}),
                frame_id: None,
                resource_type: SubresourceResourceType::Fetch,
                keepalive: false,
            },
        };
        (producer, receive)
    }

    #[tokio::test]
    async fn discarded_auth_response_retains_queued_bytes_without_completing_the_request() {
        let cancel = FetchCancelHandle::new();
        let (producer, mut receive) = producer_for_test(cancel.clone());
        let resource = producer.response.clone();
        let load = producer.load.clone();
        let mut head = crate::network_host::local_url_response(&Url::parse("data:,auth").unwrap())
            .unwrap()
            .head();
        head.status = 401;
        head.headers = vec![("www-authenticate".into(), "Basic realm=held".into())];
        resource.configure_interception(false, true);
        resource.response_started(ResourceResponseHead {
            status_text: None,
            head: head.clone(),
            network_request_headers: None,
        });
        let (chunks, receiver) = mpsc::unbounded_channel();
        chunks.send(vec![0, 128]).unwrap();
        chunks.send(vec![255, 65]).unwrap();
        let (_finished, completion) = tokio::sync::oneshot::channel();
        let response =
            StreamingRawResponse::new_with_head(head, receiver, cancel.clone(), completion);
        let paused = WorkerFetchPausedResponse::Auth(Box::new(WorkerFetchAuthResponse {
            transfer: Some((producer, response, true)),
        }));
        assert_eq!(paused.discard().status, 401);
        assert!(cancel.is_cancelled());
        assert!(
            !load.is_cancelled(),
            "the same admission must still allow the authentication retry"
        );
        assert!(
            receive.try_recv().is_err(),
            "discard does not invent a request completion"
        );
        let ResourceResponseFailure::PartialBody { response, body, .. } =
            resource.failure("stopped".into())
        else {
            panic!("received head and bytes must survive")
        };
        assert_eq!(response.head.status, 401);
        assert_eq!(body.clone_body_bytes(), [0, 128, 255, 65]);
    }
}

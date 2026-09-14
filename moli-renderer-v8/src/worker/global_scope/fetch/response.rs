use super::*;

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
        mut self,
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
            let mut stream_sender = None;
            let mut network_request_headers = None;
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
                        network_request_headers = headers;
                        let mut head = response.head();
                        if !redirects.is_empty() {
                            let mut chain = redirects;
                            chain.append(&mut head.redirect_chain);
                            head.redirect_chain = chain;
                            head.redirected = true;
                        }
                        self.response.response_started(ResourceResponseHead {
                            status_text: None,
                            head: head.clone(),
                            network_request_headers: network_request_headers.clone(),
                        });
                        if stream_to_script && !self.response.intercepts_response(&head) {
                            let id = crate::network_host::new_network_body_source_id();
                            let sender = self.stream_sender(id);
                            sender.streaming_started(head);
                            stream_sender = Some(sender);
                        }
                        while let Some(bytes) = response.next_chunk().await {
                            self.response.data_received(&bytes);
                            if let Some(sender) = &stream_sender {
                                sender.streaming_chunk(bytes);
                            }
                        }
                        match response.finish().await {
                            Ok(()) => {
                                Ok(self.response.finish_response().expect("received response"))
                            }
                            Err(error) => Err(self.response.failure(format!("fetch: {error}"))),
                        }
                    }
                    Err(error) => {
                        let message = format!("fetch: {error}");
                        Err(error.with_message(message))
                    }
                }
            };
            self.complete(result, network_request_headers);
        });
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
        let client =
            crate::network::ResourceRequestClient::new(&moli_fetch::FetchConfig::default())
                .unwrap();
        for streamed in [false, true] {
            let cancel = FetchCancelHandle::new();
            let load = crate::network::loads::resource_load_lease_for_test(
                client.handle(),
                Some(cancel.clone()),
            );
            let response = ResourceResponseStream::unobserved_for_test();
            let (send, mut receive) = mpsc::unbounded_channel();
            let mut producer = WorkerFetchCompletionSender {
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
}

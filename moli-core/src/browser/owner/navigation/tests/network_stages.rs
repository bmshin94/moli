use super::*;
use crate::browser::{NetworkOwner, NetworkRequestState, WorkerHandle};
use crate::page::{
    RendererNetworkOutputItem, ScriptNetworkOutputItem, SubresourceBodyFinishedResult,
};

#[derive(Clone, Copy, Debug)]
enum WorkerKind {
    Dedicated,
    Nested,
    Shared,
    Service,
}

#[derive(Clone, Copy, Debug)]
enum RequestKind {
    Fetch,
    Xhr,
    SyncXhr,
}

#[derive(Clone, Copy, Debug)]
enum Finish {
    Complete,
    PartialFailure,
    DetachedKeepalive,
    DetachedBeforeHeadersFailure,
    RetiredCancellation,
}

impl Finish {
    fn keepalive(self) -> bool {
        matches!(
            self,
            Self::DetachedKeepalive | Self::DetachedBeforeHeadersFailure
        )
    }

    fn retires_before_headers(self) -> bool {
        self.keepalive() || matches!(self, Self::RetiredCancellation)
    }
}

#[tokio::test]
async fn native_worker_stages_dedicated_fetch() {
    worker_network_stages(WorkerKind::Dedicated, Finish::Complete).await;
}

#[tokio::test]
async fn native_worker_stages_nested_fetch() {
    worker_network_stages(WorkerKind::Nested, Finish::Complete).await;
}

#[tokio::test]
async fn native_worker_stages_shared_fetch() {
    worker_network_stages(WorkerKind::Shared, Finish::Complete).await;
}

#[tokio::test]
async fn native_worker_stages_service_fetch() {
    worker_network_stages(WorkerKind::Service, Finish::Complete).await;
}

#[tokio::test]
async fn native_worker_stages_partial_failure_retains_received_body() {
    worker_network_stages(WorkerKind::Dedicated, Finish::PartialFailure).await;
}

#[tokio::test]
async fn native_worker_stages_detached_keepalive_keeps_original_request_and_releases_it() {
    worker_network_stages(WorkerKind::Dedicated, Finish::DetachedKeepalive).await;
}

#[tokio::test]
async fn native_worker_stages_nested_detached_keepalive() {
    worker_network_stages(WorkerKind::Nested, Finish::DetachedKeepalive).await;
}

#[tokio::test]
async fn native_worker_stages_shared_detached_keepalive() {
    worker_network_stages(WorkerKind::Shared, Finish::DetachedKeepalive).await;
}

#[tokio::test]
async fn native_worker_stages_service_detached_keepalive() {
    worker_network_stages(WorkerKind::Service, Finish::DetachedKeepalive).await;
}

#[tokio::test]
async fn native_worker_stages_detached_failure_before_headers() {
    worker_network_stages(WorkerKind::Dedicated, Finish::DetachedBeforeHeadersFailure).await;
}

#[tokio::test]
async fn native_worker_stages_dedicated_xhr() {
    worker_network_stages_with_request(WorkerKind::Dedicated, Finish::Complete, RequestKind::Xhr)
        .await;
}

#[tokio::test]
async fn native_worker_stages_nested_xhr() {
    worker_network_stages_with_request(WorkerKind::Nested, Finish::Complete, RequestKind::Xhr)
        .await;
}

#[tokio::test]
async fn native_worker_stages_shared_xhr() {
    worker_network_stages_with_request(WorkerKind::Shared, Finish::Complete, RequestKind::Xhr)
        .await;
}

#[tokio::test]
async fn native_worker_stages_dedicated_sync_xhr() {
    worker_network_stages_with_request(
        WorkerKind::Dedicated,
        Finish::Complete,
        RequestKind::SyncXhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_shared_sync_xhr() {
    worker_network_stages_with_request(WorkerKind::Shared, Finish::Complete, RequestKind::SyncXhr)
        .await;
}

#[tokio::test]
async fn native_worker_stages_xhr_partial_failure_retains_received_body() {
    worker_network_stages_with_request(
        WorkerKind::Dedicated,
        Finish::PartialFailure,
        RequestKind::Xhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_sync_xhr_partial_failure_retains_received_body() {
    worker_network_stages_with_request(
        WorkerKind::Dedicated,
        Finish::PartialFailure,
        RequestKind::SyncXhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_dedicated_xhr_retirement_cancels_request() {
    worker_network_stages_with_request(
        WorkerKind::Dedicated,
        Finish::RetiredCancellation,
        RequestKind::Xhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_nested_xhr_retirement_cancels_request() {
    worker_network_stages_with_request(
        WorkerKind::Nested,
        Finish::RetiredCancellation,
        RequestKind::Xhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_shared_xhr_retirement_cancels_request() {
    worker_network_stages_with_request(
        WorkerKind::Shared,
        Finish::RetiredCancellation,
        RequestKind::Xhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_dedicated_sync_xhr_retirement_cancels_request() {
    worker_network_stages_with_request(
        WorkerKind::Dedicated,
        Finish::RetiredCancellation,
        RequestKind::SyncXhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_nested_sync_xhr_retirement_cancels_request() {
    worker_network_stages_with_request(
        WorkerKind::Nested,
        Finish::RetiredCancellation,
        RequestKind::SyncXhr,
    )
    .await;
}

#[tokio::test]
async fn native_worker_stages_shared_sync_xhr_retirement_cancels_request() {
    worker_network_stages_with_request(
        WorkerKind::Shared,
        Finish::RetiredCancellation,
        RequestKind::SyncXhr,
    )
    .await;
}

async fn worker_network_stages(kind: WorkerKind, finish: Finish) {
    worker_network_stages_with_request(kind, finish, RequestKind::Fetch).await;
}

async fn worker_network_stages_with_request(
    kind: WorkerKind,
    finish: Finish,
    request_kind: RequestKind,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let url = format!("{origin}/probe");
    let (requested, request_arrived) = oneshot::channel();
    let (headers, release_headers) = oneshot::channel();
    let (chunk, release_chunk) = oneshot::channel();
    let (tail, release_tail) = oneshot::channel();
    let server = tokio::spawn(async move {
        let request_script = match request_kind {
            RequestKind::Fetch => format!(
                "fetch('/probe',{{keepalive:{}}}).then(r=>r.text()).catch(()=>{{}})",
                finish.keepalive()
            ),
            RequestKind::Xhr | RequestKind::SyncXhr => format!(
                "try{{const xhr=new XMLHttpRequest();xhr.open('GET','/probe',{});xhr.send();}}catch(_){{}}",
                matches!(request_kind, RequestKind::Xhr)
            ),
        };
        let worker_script = match kind {
            WorkerKind::Dedicated => request_script.clone(),
            WorkerKind::Nested => "globalThis.child = new Worker('/nested.js')".into(),
            WorkerKind::Shared => format!("onconnect=()=>{{{request_script}}}"),
            WorkerKind::Service => {
                assert!(matches!(request_kind, RequestKind::Fetch));
                format!("addEventListener('install',event=>event.waitUntil({request_script}))")
            }
        };
        let bootstrap = match kind {
            WorkerKind::Dedicated | WorkerKind::Nested => {
                "globalThis.worker = new Worker('/worker.js')"
            }
            WorkerKind::Shared => {
                "globalThis.worker = new SharedWorker('/worker.js');worker.port.start()"
            }
            WorkerKind::Service => "navigator.serviceWorker.register('/worker.js')",
        };
        let html = format!("<!doctype html><script>{bootstrap}</script>");
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            let path = request.split_whitespace().nth(1).unwrap();
            if path == "/probe" {
                requested.send(()).unwrap();
                release_headers.await.unwrap();
                if matches!(
                    finish,
                    Finish::DetachedBeforeHeadersFailure | Finish::RetiredCancellation
                ) {
                    break;
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.unwrap();
                release_chunk.await.unwrap();
                stream.write_all(b"bo").await.unwrap();
                release_tail.await.unwrap();
                if !matches!(finish, Finish::PartialFailure) {
                    stream.write_all(b"dy").await.unwrap();
                }
                break;
            }
            let (content_type, body) = match path {
                "/" => ("text/html", html.as_str()),
                "/worker.js" => ("text/javascript", worker_script.as_str()),
                "/nested.js" => ("text/javascript", request_script.as_str()),
                other => panic!("unexpected Worker fixture request: {other}"),
            };
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let service = BrowserService::start().unwrap();
    let browser = service.handle();
    let (context, contents) = context_with_contents(&service);
    let (_, mut events) = browser.subscribe().unwrap();
    navigate(&context, contents, &format!("{origin}/")).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), request_arrived)
        .await
        .expect("the real Worker must dispatch its request before any response is released")
        .unwrap();
    let (owner, source, handle, mut sequence) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            loop {
                let event = events.recv().await.unwrap();
                if let BrowserEvent::NetworkRequestStarted(occurrence) = event.event
                    && let NetworkOwner::Worker(owner) = occurrence.owner
                    && let RendererNetworkOutputItem::Resource(item) = &occurrence.renderer.item
                    && let ScriptNetworkOutputItem::SubresourceRequestStarted(request) =
                        item.as_ref()
                    && request.url().as_str() == url
                {
                    assert_eq!(request.keepalive(), finish.keepalive());
                    assert_eq!(
                        request.resource_type(),
                        match request_kind {
                            RequestKind::Fetch => crate::page::SubresourceResourceType::Fetch,
                            RequestKind::Xhr | RequestKind::SyncXhr =>
                                crate::page::SubresourceResourceType::Xhr,
                        }
                    );
                    break (
                        owner,
                        occurrence.renderer.source.clone(),
                        request.handle(),
                        event.sequence,
                    );
                }
            }
        },
    )
    .await
    .expect(
        "native Worker Started must precede the held response headers without a DevTools consumer",
    );
    assert!(matches!(
        (kind, owner),
        (
            WorkerKind::Dedicated | WorkerKind::Nested,
            WorkerHandle::Dedicated { .. }
        ) | (WorkerKind::Shared, WorkerHandle::Shared { .. })
            | (WorkerKind::Service, WorkerHandle::Service { .. })
    ));
    assert!(browser.subscribe().unwrap().0.network_requests.iter().any(|request|
        request.owner == NetworkOwner::Worker(owner) && request.renderer_source == source
        && matches!(&request.state, NetworkRequestState::Started(start) if start.handle() == handle)));
    let mut buffered = std::collections::VecDeque::new();
    if finish.retires_before_headers() {
        if let crate::page::RendererNetworkSource::Worker(
            crate::page::RendererWorkerIdentity::Service { run, .. },
        ) = &source
        {
            assert!(browser.subscribe().unwrap().0.workers.iter().any(|worker|
                worker.handle() == owner && matches!(worker, crate::browser::WorkerSnapshot::Service { worker, .. } if worker.execution.active_run() == Some(run))));
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            context.close_web_contents(contents).unwrap().close_async(),
        )
        .await
        .expect("Worker retirement must finish with the response headers held");
        if let WorkerHandle::Service { version, .. } = owner {
            context
                .execute_service_worker_command(crate::browser::ServiceWorkerCommand::StopVersion {
                    version_id: version,
                })
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                let retired = match &event.event {
                    BrowserEvent::WorkerDestroyed(closed) => *closed == owner,
                    BrowserEvent::WorkerUpdated(worker) if worker.handle() == owner => matches!(worker,
                        crate::browser::WorkerSnapshot::Service { worker, .. } if worker.execution == crate::browser::ServiceWorkerExecution::Stopped),
                    _ => false,
                };
                if retired {
                    assert!(event.sequence > sequence);
                    break;
                }
                // Cancellation can finish before the WorkerDestroyed fact.
                // Preserve the real FIFO instead of discarding that completion.
                buffered.push_back(event);
            }
        })
        .await
        .expect("the original Worker must retire while its response is held");
        assert!(
            browser
                .subscribe()
                .unwrap()
                .0
                .workers
                .iter()
                .all(|worker| worker.handle() != owner || matches!(worker,
                    crate::browser::WorkerSnapshot::Service { worker, .. } if worker.execution == crate::browser::ServiceWorkerExecution::Stopped))
        );
    }
    let mut headers = Some(headers);
    if !matches!(finish, Finish::RetiredCancellation) {
        headers.take().unwrap().send(()).unwrap();
    }
    let stages = if matches!(
        finish,
        Finish::DetachedBeforeHeadersFailure | Finish::RetiredCancellation
    ) {
        vec![(2, None)]
    } else {
        vec![(0, Some(chunk)), (1, Some(tail)), (2, None)]
    };
    for (stage, release) in stages {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = match buffered.pop_front() {
                    Some(event) => event,
                    None => events.recv().await.unwrap(),
                };
                let occurrence = match &event.event {
                    BrowserEvent::NetworkActivity(occurrence)
                    | BrowserEvent::NetworkRequestCompleted(occurrence)
                        if occurrence.owner == NetworkOwner::Worker(owner) => occurrence,
                    _ => continue,
                };
                assert_eq!(occurrence.renderer.source, source);
                let RendererNetworkOutputItem::Resource(item) = &occurrence.renderer.item else { continue };
                let matches = match (stage, item.as_ref()) {
                    (0, ScriptNetworkOutputItem::SubresourceResponseStarted(response)) => response.handle() == handle,
                    (1, ScriptNetworkOutputItem::SubresourceDataReceived(data)) => data.handle() == handle,
                    (2, ScriptNetworkOutputItem::SubresourceBodyFinished(body)) => body.handle() == handle,
                    _ => false,
                };
                if matches { break event; }
            }
        }).await.unwrap_or_else(|_| panic!("Worker {kind:?} {finish:?} must publish stage {stage} before the next transport gate opens"));
        assert!(event.sequence > sequence);
        sequence = event.sequence;
        match stage {
            0 => {
                assert!(browser.subscribe().unwrap().0.network_requests.iter().any(|request|
                    request.owner == NetworkOwner::Worker(owner) && request.renderer_source == source
                    && matches!(&request.state, NetworkRequestState::Responding { response, .. } if response.handle() == handle)));
            }
            1 => {}
            2 => {
                let BrowserEvent::NetworkRequestCompleted(occurrence) = event.event else {
                    panic!("the final body is a native completion, not generic activity");
                };
                let RendererNetworkOutputItem::Resource(item) = &occurrence.renderer.item else {
                    unreachable!()
                };
                let ScriptNetworkOutputItem::SubresourceBodyFinished(body) = item.as_ref() else {
                    unreachable!()
                };
                match (finish, body.result()) {
                    (
                        Finish::DetachedBeforeHeadersFailure | Finish::RetiredCancellation,
                        SubresourceBodyFinishedResult::Failed(error),
                    ) => assert!(!error.is_empty()),
                    (
                        Finish::Complete | Finish::DetachedKeepalive,
                        SubresourceBodyFinishedResult::Ready(body),
                    ) => assert_eq!(body.clone_body_bytes(), b"body"),
                    (
                        Finish::PartialFailure,
                        SubresourceBodyFinishedResult::FailedWithPartialBody {
                            error_text,
                            partial_body,
                        },
                    ) => {
                        assert!(!error_text.is_empty());
                        assert_eq!(partial_body.clone_body_bytes(), b"bo");
                    }
                    other => {
                        panic!("native terminal must retain the actual transport result: {other:?}")
                    }
                }
            }
            _ => unreachable!(),
        }
        if let Some(release) = release {
            release.send(()).unwrap();
        }
    }
    // Only let the server close after observing native cancellation. Otherwise
    // a fixture-induced disconnect could falsely prove retirement cancellation.
    if let Some(headers) = headers {
        headers.send(()).unwrap();
    }
    server.await.unwrap();
    if let WorkerHandle::Service { version, .. } = owner
        && !finish.retires_before_headers()
    {
        context
            .execute_service_worker_command(crate::browser::ServiceWorkerCommand::StopVersion {
                version_id: version,
            })
            .unwrap();
    }
    if !finish.retires_before_headers() {
        context
            .close_web_contents(contents)
            .unwrap()
            .close_async()
            .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if browser
                .subscribe()
                .unwrap()
                .0
                .network_requests
                .iter()
                .all(|request| request.renderer_source != source)
            {
                break;
            }
            events.recv().await.unwrap();
        }
    })
    .await
    .expect(
        "retired physical source and its completed keepalive tail must release native retention",
    );
    service.shutdown();
}

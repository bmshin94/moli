use super::*;
use crate::runtime::RendererNetworkOutputItem;
use moli_page_types::{ScriptNetworkOutputItem, SubresourceBodyFinishedResult};

#[tokio::test]
async fn accepted_fetch_response_streams_without_interception() {
    accepted_response(SubresourceResourceType::Fetch, false, false).await;
}

#[tokio::test]
async fn accepted_xhr_response_streams_without_interception() {
    accepted_response(SubresourceResourceType::Xhr, false, false).await;
}

#[tokio::test]
async fn accepted_fetch_auth_response_streams_before_eof() {
    accepted_response(SubresourceResourceType::Fetch, true, false).await;
}

#[tokio::test]
async fn accepted_xhr_auth_response_streams_before_eof() {
    accepted_response(SubresourceResourceType::Xhr, true, false).await;
}

#[tokio::test]
async fn accepted_fetch_auth_response_retains_partial_body() {
    accepted_response(SubresourceResourceType::Fetch, true, true).await;
}

#[tokio::test]
async fn accepted_xhr_auth_response_retains_partial_body() {
    accepted_response(SubresourceResourceType::Xhr, true, true).await;
}

async fn accepted_response(resource_type: SubresourceResourceType, auth: bool, partial: bool) {
    ensure_v8();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (prefix_tx, prefix_rx) = tokio::sync::oneshot::channel();
    let (tail_tx, tail_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        for authenticated in if auth { vec![false, true] } else { vec![false] } {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request_head(&mut stream).await.unwrap();
            assert!(request.starts_with("POST /probe "));
            assert_eq!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: basic "),
                authenticated
            );
            let mut body = [0; 4];
            tokio::io::AsyncReadExt::read_exact(&mut stream, &mut body)
                .await
                .unwrap();
            assert_eq!(body, [0, 128, 255, 65]);
            if auth && !authenticated {
                stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"worker-stage\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                continue;
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.unwrap();
            // Each gate requires a receipt from the exact request. No body byte
            // can make a buffered response look like a streaming response.
            if prefix_rx.await.is_err() {
                return;
            }
            stream.write_all(b"bo").await.unwrap();
            if tail_rx.await.is_err() {
                return;
            }
            if !partial {
                stream.write_all(b"dy").await.unwrap();
            }
            return;
        }
    });
    let is_fetch = resource_type == SubresourceResourceType::Fetch;
    let script = if is_fetch {
        "onmessage=async()=>{try{const r=await fetch('/probe',{method:'POST',body:new Uint8Array([0,128,255,65])});postMessage('headers');postMessage(await r.text());}catch(e){postMessage('rejected');}close();};"
    } else {
        "onmessage=()=>{const x=new XMLHttpRequest();x.open('POST','/probe');x.onload=()=>{postMessage(x.responseText);close();};x.onerror=()=>{postMessage('rejected');close();};x.send(new Uint8Array([0,128,255,65]));};"
    };
    let mut worker = spawn_worker_with_request_client(
        script.into(),
        format!("{origin}/worker.js"),
        ResourceRequestClient::new(&FetchConfig::default()).unwrap(),
    );
    worker.set_fetch_subresource_interception(auth, Some(resource_type));
    worker.post_message(serialize_test_string("go"));
    let mut prefix_tx = Some(prefix_tx);
    let mut tail_tx = Some(tail_tx);
    let mut handle = None;
    let mut heads = 0;
    let mut bytes = 0;
    let mut terminals = 0;
    let mut posts = Vec::new();
    timeout(TIMEOUT, async {
        while let Some(message) = worker.recv().await {
            match message {
                WorkerToParentMessage::Network(observation) => {
                    let RendererNetworkOutputItem::Resource(item) = observation.item() else {
                        panic!("resource receipt")
                    };
                    match item.as_ref() {
                        ScriptNetworkOutputItem::SubresourceRequestStarted(start) => {
                            assert!(
                                handle.replace(start.handle()).is_none(),
                                "one admission across auth rounds"
                            );
                        }
                        ScriptNetworkOutputItem::SubresourceResponseStarted(head) => {
                            assert_eq!(Some(head.handle()), handle);
                            assert_eq!(
                                head.status(),
                                200,
                                "the challenge remains private to the auth decision"
                            );
                            assert_initial_worker_auth_network_headers(
                                head.network_request_headers(),
                            );
                            heads += 1;
                            assert_eq!(heads, 1);
                        }
                        ScriptNetworkOutputItem::SubresourceDataReceived(data) => {
                            assert_eq!(Some(data.handle()), handle);
                            assert_eq!(heads, 1);
                            assert_eq!(terminals, 0);
                            assert_eq!(data.data_length(), 2);
                            bytes += data.data_length();
                            if let Some(release) = tail_tx.take() {
                                release.send(()).unwrap();
                            }
                        }
                        ScriptNetworkOutputItem::SubresourceBodyFinished(terminal) => {
                            assert_eq!(Some(terminal.handle()), handle);
                            assert_eq!(heads, 1);
                            terminals += 1;
                            assert_eq!(terminals, 1);
                            match terminal.result() {
                                SubresourceBodyFinishedResult::Ready(body) if !partial => {
                                    assert_eq!(body.clone_body_bytes(), b"body");
                                    assert!(terminal.data_was_streamed());
                                }
                                SubresourceBodyFinishedResult::FailedWithPartialBody {
                                    partial_body,
                                    ..
                                } if partial => {
                                    assert_eq!(partial_body.clone_body_bytes(), b"bo");
                                }
                                result => panic!("unexpected terminal: {result:?}"),
                            }
                        }
                        other => panic!("unexpected resource stage: {other:?}"),
                    }
                }
                WorkerToParentMessage::FetchInterception(pause) => {
                    assert!(auth);
                    match pause.stage() {
                        crate::runtime::RendererWorkerFetchStage::Request(_) => {
                            continue_worker_request(&pause, false, true).await
                        }
                        crate::runtime::RendererWorkerFetchStage::Auth(info) => {
                            assert_eq!(info.challenge.realm, "worker-stage");
                            assert_initial_worker_auth_network_headers(
                                info.network_request_headers.as_deref(),
                            );
                            decide_worker_pause(
                                &pause,
                                crate::runtime::WorkerFetchDecision::ProvideAuth(
                                    server_basic_auth_credentials(),
                                ),
                            )
                            .await;
                        }
                        other => panic!("accepted response must not pause: {other:?}"),
                    }
                }
                WorkerToParentMessage::Post(payload) => posts.push(stringify_payload(&payload)),
                other => panic!("unexpected Worker output: {other:?}"),
            }
            if heads == 1
                && (!is_fetch || posts.iter().any(|post| post == "\"headers\""))
                && let Some(release) = prefix_tx.take()
            {
                release.send(()).unwrap();
            }
        }
    })
    .await
    .expect("native head and fetch JS headers must arrive before body release");
    worker.terminate_and_join();
    server.await.unwrap();
    assert_eq!(
        (heads, bytes, terminals),
        (1, if partial { 2 } else { 4 }, 1)
    );
    let mut expected = if is_fetch {
        vec!["\"headers\""]
    } else {
        Vec::new()
    };
    expected.push(if partial { "\"rejected\"" } else { "\"body\"" });
    assert_eq!(posts, expected);
}

#[tokio::test]
async fn intercepted_fetch_failure_retains_physical_head_and_prefix() {
    intercepted_failure(SubresourceResourceType::Fetch).await;
}

#[tokio::test]
async fn intercepted_xhr_failure_retains_physical_head_and_prefix() {
    intercepted_failure(SubresourceResourceType::Xhr).await;
}

async fn intercepted_failure(resource_type: SubresourceResourceType) {
    ensure_v8();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request_head(&mut stream).await.unwrap();
        assert!(request.starts_with("GET /partial "));
        stream.write_all(b"HTTP/1.1 200 OK\r\nX-Physical: retained\r\nContent-Length: 6\r\nConnection: close\r\n\r\npre").await.unwrap();
    });
    let script = match resource_type {
        SubresourceResourceType::Fetch => {
            "onmessage=async()=>{try{const r=await fetch('/partial');await r.text();postMessage('resolved');}catch(e){postMessage('rejected');}close();};"
        }
        SubresourceResourceType::Xhr => {
            "onmessage=()=>{const x=new XMLHttpRequest();x.open('GET','/partial');x.onload=()=>{postMessage('resolved');close();};x.onerror=()=>{postMessage('rejected');close();};x.send();};"
        }
        _ => unreachable!(),
    };
    let mut worker = spawn_worker_with_request_client(
        script.into(),
        format!("{origin}/worker.js"),
        ResourceRequestClient::new(&FetchConfig::default()).unwrap(),
    );
    worker.set_fetch_subresource_interception(true, Some(resource_type));
    worker.post_message(serialize_test_string("go"));
    let mut items = Vec::new();
    let mut posts = Vec::new();
    timeout(TIMEOUT, async {
        while let Some(message) = worker.recv().await {
            match message {
                WorkerToParentMessage::Network(observation) => {
                    let RendererNetworkOutputItem::Resource(item) = observation.item() else {
                        panic!("resource receipt")
                    };
                    items.push(item.clone());
                }
                WorkerToParentMessage::FetchInterception(pause) => {
                    assert!(
                        matches!(
                            pause.stage(),
                            crate::runtime::RendererWorkerFetchStage::Request(_)
                        ),
                        "a failed transport cannot enter a response decision"
                    );
                    continue_worker_request(&pause, true, false).await;
                }
                WorkerToParentMessage::Post(payload) => posts.push(stringify_payload(&payload)),
                other => panic!("unexpected Worker output: {other:?}"),
            }
        }
    })
    .await
    .expect("Worker must settle the failed request and close");
    worker.terminate_and_join();
    server.await.unwrap();
    assert_eq!(posts, ["\"rejected\""]);
    assert_eq!(
        items.len(),
        3,
        "admission, physical head and one failed terminal: {items:?}"
    );
    let ScriptNetworkOutputItem::SubresourceRequestStarted(start) = items[0].as_ref() else {
        panic!("request admission first")
    };
    let ScriptNetworkOutputItem::SubresourceResponseStarted(head) = items[1].as_ref() else {
        panic!("the failed stream must retain its physical head")
    };
    assert_eq!(head.handle(), start.handle());
    assert_eq!(head.status(), 200);
    assert!(
        head.response_headers()
            .iter()
            .any(|(name, value)| name == "x-physical" && value == "retained")
    );
    let ScriptNetworkOutputItem::SubresourceBodyFinished(terminal) = items[2].as_ref() else {
        panic!("one terminal last")
    };
    assert_eq!(terminal.handle(), start.handle());
    let SubresourceBodyFinishedResult::FailedWithPartialBody { partial_body, .. } =
        terminal.result()
    else {
        panic!("the failed stream must retain its received bytes")
    };
    assert_eq!(partial_body.clone_body_bytes(), b"pre");
}

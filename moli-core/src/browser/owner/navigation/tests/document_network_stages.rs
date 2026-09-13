use super::*;
use crate::browser::{NetworkOwner, NetworkRequestState};
use crate::page::{
    RendererNetworkOutputItem, ScriptNetworkOutputItem, SubresourceBodyFinishedResult,
    SubresourceResourceType,
};

#[derive(Clone, Copy, Debug)]
enum Finish {
    Complete,
    PartialFailure,
    PageClosed,
    ChildRemoved,
    DocumentOpened,
}

macro_rules! csp_stage_tests {
    ($($name:ident: $child:literal, $finish:ident;)*) => {
        $(#[tokio::test]
        async fn $name() {
            document_csp_stages($child, Finish::$finish).await;
        })*
    };
}

csp_stage_tests! {
    native_document_csp_stages_top: false, Complete;
    native_document_csp_stages_top_partial: false, PartialFailure;
    native_document_csp_stages_top_closed: false, PageClosed;
    native_document_csp_stages_child: true, Complete;
    native_document_csp_stages_child_partial: true, PartialFailure;
    native_document_csp_stages_child_page_closed: true, PageClosed;
    native_document_csp_stages_child_removed: true, ChildRemoved;
    native_document_csp_stages_top_opened: false, DocumentOpened;
    native_document_csp_stages_child_opened: true, DocumentOpened;
}

async fn document_csp_stages(child: bool, finish: Finish) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let report_url = format!("{origin}/report");
    let (requested, request_arrived) = tokio::sync::oneshot::channel();
    let (headers, release_headers) = tokio::sync::oneshot::channel();
    let (chunk, release_chunk) = tokio::sync::oneshot::channel();
    let (tail, release_tail) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
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
            if path == "/report" {
                assert!(request.starts_with("POST /report HTTP/1.1"));
                let length = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .expect("CSP report body length");
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(report["csp-report"]["effective-directive"], "connect-src");
                requested.send(()).unwrap();
                if release_headers.await.is_err() {
                    return;
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.unwrap();
                if release_chunk.await.is_err() {
                    return;
                }
                stream.write_all(b"bo").await.unwrap();
                if release_tail.await.is_err() {
                    return;
                }
                if !matches!(finish, Finish::PartialFailure) {
                    stream.write_all(b"dy").await.unwrap();
                }
                break;
            }
            let report_document = path == "/child" || (path == "/page" && !child);
            assert!(
                path == "/page" || (path == "/child" && child),
                "unexpected request: {path}"
            );
            let html = if report_document {
                "<!doctype html><script>fetch('/blocked').catch(()=>{})</script>"
            } else {
                "<!doctype html><iframe src='/child'></iframe>"
            };
            let csp = if report_document {
                "Content-Security-Policy: connect-src 'none'; report-uri /report\r\n"
            } else {
                ""
            };
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{csp}Content-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes()).await.unwrap();
        }
    });
    let service = BrowserService::start().unwrap();
    let browser = service.handle();
    let (context, contents) = context_with_contents(&service);
    let (_, mut events) = browser.subscribe().unwrap();
    let document = navigate(&context, contents, &format!("{origin}/page")).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), request_arrived)
        .await
        .expect("the real CSP report must reach HTTP with headers held")
        .unwrap();
    let (source, handle, mut sequence) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if let BrowserEvent::NetworkRequestStarted(occurrence) = event.event
                    && occurrence.owner == NetworkOwner::Document(document)
                    && let RendererNetworkOutputItem::Resource(item) = &occurrence.renderer.item
                    && let ScriptNetworkOutputItem::SubresourceRequestStarted(request) =
                        item.as_ref()
                    && request.url().as_str() == report_url
                {
                    assert_eq!(request.method(), "POST");
                    assert_eq!(request.resource_type(), SubresourceResourceType::CspReport);
                    assert!(request.keepalive());
                    break (
                        occurrence.renderer.source.clone(),
                        request.handle(),
                        event.sequence,
                    );
                }
            }
        })
        .await
        .expect("CSP native admission must precede held response headers without DevTools");
    if matches!(finish, Finish::PageClosed) {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            context.close_web_contents(contents).unwrap().close_async(),
        )
        .await
        .expect("Page closes while its CSP report is pending");
        assert!(
            !browser
                .subscribe()
                .unwrap()
                .0
                .web_contents
                .contains(&contents)
        );
    } else if matches!(finish, Finish::DocumentOpened) {
        assert_eq!(context.evaluate_document_expression_for_test(
            document, "document.open();document.write('<!doctype html><title>replacement</title>');document.close();document.title", false,
        ).await.unwrap()["value"], "replacement");
    } else if matches!(finish, Finish::ChildRemoved) {
        let before = context
            .start_child_frame_tree_snapshot(document)
            .unwrap()
            .wait()
            .await;
        assert_eq!(
            context
                .finish_child_frame_tree_snapshot(before)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(context.evaluate_document_expression_for_test(
            document, "document.querySelector('iframe').remove();document.querySelector('iframe')===null", false,
        ).await.unwrap()["value"], true);
        let after = context
            .start_child_frame_tree_snapshot(document)
            .unwrap()
            .wait()
            .await;
        assert!(
            context
                .finish_child_frame_tree_snapshot(after)
                .unwrap()
                .is_empty(),
            "the original child must actually leave the frame tree"
        );
    }
    headers.send(()).unwrap();
    for (stage, release) in [(0, Some(chunk)), (1, Some(tail)), (2, None)] {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if let BrowserEvent::NetworkSourceClosed { source: closed, .. } = &event.event {
                    assert_ne!(*closed, source.identity(), "source closure cannot overtake an admitted CSP report");
                }
                let occurrence = match &event.event {
                    BrowserEvent::NetworkActivity(occurrence)
                    | BrowserEvent::NetworkRequestCompleted(occurrence)
                        if occurrence.owner == NetworkOwner::Document(document) => occurrence,
                    _ => continue,
                };
                if occurrence.renderer.source != source { continue; }
                let RendererNetworkOutputItem::Resource(item) = &occurrence.renderer.item else { continue; };
                if let ScriptNetworkOutputItem::SubresourceBodyFinished(body) = item.as_ref()
                    && body.handle() == handle && stage < 2 {
                    panic!("CSP terminal overtook held stage {stage}: {:?}", body.result());
                }
                let matched = match (stage, item.as_ref()) {
                    (0, ScriptNetworkOutputItem::SubresourceResponseStarted(head)) => head.handle() == handle,
                    (1, ScriptNetworkOutputItem::SubresourceDataReceived(data)) => data.handle() == handle,
                    (2, ScriptNetworkOutputItem::SubresourceBodyFinished(body)) => body.handle() == handle,
                    _ => false,
                };
                if matched { break event; }
            }
        }).await.unwrap_or_else(|_| panic!("CSP child={child} {finish:?}: native stage {stage} must precede the next transport gate"));
        assert!(event.sequence > sequence);
        sequence = event.sequence;
        let occurrence = match event.event {
            BrowserEvent::NetworkActivity(occurrence) if stage < 2 => occurrence,
            BrowserEvent::NetworkRequestCompleted(occurrence) if stage == 2 => occurrence,
            other => panic!("wrong native phase event: {other:?}"),
        };
        let RendererNetworkOutputItem::Resource(item) = &occurrence.renderer.item else {
            unreachable!()
        };
        match item.as_ref() {
            ScriptNetworkOutputItem::SubresourceResponseStarted(head) => {
                assert_eq!(head.status(), 200);
                assert!(browser.subscribe().unwrap().0.network_requests.iter().any(|request|
                    request.owner == NetworkOwner::Document(document) && request.renderer_source == source
                    && matches!(&request.state, NetworkRequestState::Responding { response, .. } if response.handle() == handle)));
            }
            ScriptNetworkOutputItem::SubresourceDataReceived(data) => {
                assert_eq!(data.data_length(), 2)
            }
            ScriptNetworkOutputItem::SubresourceBodyFinished(body) => match (finish, body.result())
            {
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
                (
                    Finish::Complete
                    | Finish::PageClosed
                    | Finish::ChildRemoved
                    | Finish::DocumentOpened,
                    SubresourceBodyFinishedResult::Ready(body),
                ) => assert_eq!(body.clone_body_bytes(), b"body"),
                other => panic!("native terminal must retain the physical CSP result: {other:?}"),
            },
            _ => unreachable!(),
        }
        if let Some(release) = release {
            release.send(()).unwrap();
        }
    }
    server.await.unwrap();
    if !matches!(finish, Finish::PageClosed) {
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
    .expect("retired Page source releases the completed CSP tail");
    service.shutdown();
}

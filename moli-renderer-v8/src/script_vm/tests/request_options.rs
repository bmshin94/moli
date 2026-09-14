use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone, Copy)]
enum FetchOption {
    RedirectError,
    NoReferrer,
    OriginReferrer,
    DocumentReferrer,
}

#[tokio::test]
async fn continued_window_fetch_retains_redirect_error() {
    window_fetch_option(FetchOption::RedirectError, true, false).await;
}

#[tokio::test]
async fn authenticated_window_fetch_retains_redirect_error() {
    window_fetch_option(FetchOption::RedirectError, true, true).await;
}

#[tokio::test]
async fn continued_window_fetch_retains_no_referrer() {
    window_fetch_option(FetchOption::NoReferrer, true, false).await;
}

#[tokio::test]
async fn authenticated_window_fetch_retains_no_referrer() {
    window_fetch_option(FetchOption::NoReferrer, true, true).await;
}

#[tokio::test]
async fn continued_window_fetch_retains_origin_referrer() {
    window_fetch_option(FetchOption::OriginReferrer, true, false).await;
}

#[tokio::test]
async fn authenticated_window_fetch_retains_origin_referrer() {
    window_fetch_option(FetchOption::OriginReferrer, true, true).await;
}

#[tokio::test]
async fn continued_window_fetch_retains_original_document_referrer_policy() {
    window_fetch_option(FetchOption::DocumentReferrer, true, false).await;
}

#[tokio::test]
async fn authenticated_window_fetch_retains_original_document_referrer_policy() {
    window_fetch_option(FetchOption::DocumentReferrer, true, true).await;
}

#[tokio::test]
async fn ordinary_window_fetch_applies_original_options() {
    for option in [
        FetchOption::RedirectError,
        FetchOption::NoReferrer,
        FetchOption::OriginReferrer,
        FetchOption::DocumentReferrer,
    ] {
        window_fetch_option(option, false, false).await;
    }
}

async fn window_fetch_option(option: FetchOption, intercepted: bool, authenticate: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                _ = &mut stopped => return requests,
            };
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
            }
            let head = String::from_utf8(bytes).unwrap();
            let authorized = request_header(&head, "authorization") == Some("Basic dXNlcjpwYXNz");
            let response = if authenticate && !authorized {
                "401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\""
            } else if matches!(option, FetchOption::RedirectError)
                && head.starts_with("GET /probe ")
            {
                "307 Temporary Redirect\r\nLocation: /must-not-follow"
            } else {
                "200 OK"
            };
            requests.push(head);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {response}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let loader = ResourceRequestClient::new(&moli_fetch::FetchConfig::default()).unwrap();
    let (mut vm, mut completions) = new_storage_test_vm_with_loader_and_resource_completion_queue(
        &format!("{origin}/source/path?private=value"),
        &loader,
    );
    let output = NativeResourceOutput::observe(&vm);
    if matches!(option, FetchOption::DocumentReferrer) {
        vm.set_response_referrer_policy(Some("origin".into()));
    }
    vm.set_fetch_subresource_interception(intercepted, None);
    let options = match option {
        FetchOption::RedirectError => "{redirect:'error'}",
        FetchOption::NoReferrer => "{referrer:''}",
        FetchOption::OriginReferrer => "{referrerPolicy:'origin'}",
        FetchOption::DocumentReferrer => "{}",
    };
    vm.exec(&format!("globalThis.result='pending'; fetch('/probe',{options}).then(()=>result='resolved',error=>result=error.name);"), None).unwrap();
    let request_id = if intercepted {
        let pending = vm.take_pending_subresource_fetch_infos();
        assert_eq!(pending.len(), 1);
        let id = pending[0].internal_id;
        if matches!(option, FetchOption::DocumentReferrer) {
            vm.set_response_referrer_policy(Some("unsafe-url".into()));
        }
        vm.continue_pending_subresource_fetch(id, None, None, None, None, false, authenticate)
            .unwrap();
        Some(id)
    } else {
        None
    };
    let mut challenges = 0;
    let mut observations = Vec::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            assert!(completions.wait_for_arrival_without_timeout().await);
            while let Some(event) = completions.pop_next_async_subresource_event() {
                let _ = vm
                    .complete_async_subresource_fetch_event_body(event)
                    .unwrap();
            }
            for event in vm.take_pending_subresource_continue_events() {
                if let crate::types::PendingSubresourceContinueEvent::AuthRequired(info) = event {
                    assert!(authenticate);
                    assert_eq!(Some(info.internal_id), request_id);
                    challenges += 1;
                    assert_eq!(challenges, 1, "credentials must settle the challenge");
                    let _ = vm
                        .continue_pending_subresource_auth_body(
                            info.internal_id,
                            crate::SubresourceAuthCredentials {
                                target: crate::types::SubresourceAuthTarget::Server,
                                scheme: crate::types::SubresourceAuthScheme::Basic,
                                username: "user".into(),
                                password: "pass".into(),
                            },
                        )
                        .unwrap();
                }
            }
            vm.with_default_context_scope_and_checkpoint_for_test(|_, _| Ok(()))
                .unwrap();
            observations.extend(output.take());
            let result = vm.eval("globalThis.result").unwrap();
            if result != "pending"
                && observations.iter().any(|item| {
                    matches!(
                        item,
                        crate::types::ScriptNetworkOutputItem::SubresourceBodyFinished(_)
                    )
                })
            {
                break result;
            }
        }
    })
    .await;
    stop.send(()).unwrap();
    let requests = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let result = result.expect("request and promise must finish");
    assert_eq!(challenges, usize::from(authenticate));
    assert!(requests.len() > usize::from(authenticate));
    for head in &requests {
        assert!(
            head.starts_with("GET /probe HTTP/1.1\r\n"),
            "redirect:error must never follow: {head}"
        );
        match option {
            FetchOption::RedirectError => {}
            FetchOption::NoReferrer => assert_eq!(request_header(head, "referer"), None),
            FetchOption::OriginReferrer | FetchOption::DocumentReferrer => assert_eq!(
                request_header(head, "referer"),
                Some(format!("{origin}/").as_str())
            ),
        }
    }
    assert_eq!(
        result,
        if matches!(option, FetchOption::RedirectError) {
            "TypeError"
        } else {
            "resolved"
        }
    );
    assert_eq!(
        observations
            .iter()
            .filter(|item| matches!(
                item,
                crate::types::ScriptNetworkOutputItem::SubresourceBodyFinished(_)
            ))
            .count(),
        1,
        "all physical attempts retain a single original request terminal"
    );
}

fn request_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (header, value) = line.split_once(':')?;
        header.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn intercepted_beacon_preserves_binary_body_and_explicit_override() {
    for replacement in [None, Some("changed")] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            assert!(head.starts_with("POST /probe HTTP/1.1"));
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            body
        });
        let loader = ResourceRequestClient::new(&moli_fetch::FetchConfig::default()).unwrap();
        let (mut vm, mut completions) =
            new_storage_test_vm_with_loader_and_resource_completion_queue(&origin, &loader);
        vm.set_fetch_subresource_interception(
            true,
            Some(crate::types::SubresourceResourceType::Ping),
        );
        assert_eq!(
            vm.eval("String(navigator.sendBeacon('/probe', new Uint8Array([0,128,255,65])))")
                .unwrap(),
            "true"
        );
        let requests = vm.take_pending_subresource_fetch_infos();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(
            request.request_body_bytes.as_deref(),
            Some([0, 128, 255, 65].as_slice())
        );
        vm.continue_pending_subresource_fetch(
            request.internal_id,
            None,
            None,
            replacement.map(|body| Some(body.to_owned())),
            None,
            false,
            false,
        )
        .unwrap();
        let actual = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            actual,
            replacement.map_or_else(|| vec![0, 128, 255, 65], |body| body.as_bytes().to_vec())
        );
        // The claimed result publishes its native response and terminal. None
        // of those receipts needs the sending realm to consume a response body.
        loop {
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    completions.wait_for_arrival_without_timeout()
                )
                .await
                .unwrap()
            );
            let event = completions.pop_next_async_subresource_event().unwrap();
            let terminal = matches!(&event,
                crate::types::AsyncSubresourceFetchEvent::NativeNetwork(observation)
                    if matches!(observation.item(), crate::runtime::RendererNetworkOutputItem::Resource(item)
                        if matches!(item.as_ref(), crate::types::ScriptNetworkOutputItem::SubresourceBodyFinished(body)
                            if Some(body.handle()) == request.network_request_handle)));
            assert!(matches!(vm.complete_async_subresource_fetch_event_body(event).unwrap(),
                crate::script_vm::subresource_fetch::AsyncSubresourceFetchBodyActivity::NoWindowRealmEntered));
            if terminal {
                break;
            }
        }
        assert!(
            vm._context_host
                .borrow()
                .pending_window_beacon_execution_contexts_for_test()
                .is_empty()
        );
        assert_eq!(vm.take_network_output().into_items().filter(|item| matches!(item,
            crate::types::ScriptNetworkOutputItem::SubresourceBodyFinished(body) if Some(body.handle()) == request.network_request_handle
        )).count(), 1);
    }
}

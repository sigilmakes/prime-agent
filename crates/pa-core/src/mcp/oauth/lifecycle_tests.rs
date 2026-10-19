use super::*;

struct CancelUi {
    cancel: tokio_util::sync::CancellationToken,
    paste: std::sync::Arc<TestUi>,
}

impl McpLoginUi for CancelUi {
    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
    fn wait_cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.cancel.cancelled())
    }
    fn on_progress(&self, _message: &str) {}
    fn on_auth(&self, url: &str, instructions: &str) {
        self.paste.on_auth(url, instructions);
    }
    fn on_prompt(
        &self,
        message: &str,
        placeholder: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
        self.paste.on_prompt(message, placeholder)
    }
    fn on_manual_code_input(&self) -> Option<Pin<Box<dyn Future<Output = Option<String>> + Send>>> {
        self.paste.on_manual_code_input()
    }
}

struct HangingHttp {
    scripted: ScriptedHttp,
    hang_url: &'static str,
    entered: tokio::sync::Notify,
    dropped: std::sync::atomic::AtomicBool,
}

impl OAuthHttp for HangingHttp {
    fn request(
        &self,
        request: OAuthHttpRequest,
    ) -> futures::future::BoxFuture<'_, Result<OAuthHttpResponse>> {
        Box::pin(async move {
            struct Dropped<'a>(&'a std::sync::atomic::AtomicBool);
            impl Drop for Dropped<'_> {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            if request.url != self.hang_url {
                return self.scripted.request(request).await;
            }
            let _guard = Dropped(&self.dropped);
            self.entered.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn explicit_cancel_drops_discovery_registration_and_token_exchange() {
    for hang_url in [ORIGIN_URL, ORIGIN_REGISTER, ORIGIN_TOKEN] {
        let http = HangingHttp {
            scripted: ScriptedHttp::new(vec![
                (ORIGIN_URL, 404, None, ""),
                (
                    "https://srv.test/.well-known/oauth-protected-resource/mcp",
                    404,
                    None,
                    "",
                ),
                (ORIGIN_META_URL, 200, None, &json_response(&origin_meta())),
                (ORIGIN_REGISTER, 200, None, r#"{"client_id":"client"}"#),
            ]),
            hang_url,
            entered: tokio::sync::Notify::new(),
            dropped: std::sync::atomic::AtomicBool::new(false),
        };
        let ui = CancelUi {
            cancel: tokio_util::sync::CancellationToken::new(),
            paste: test_ui(),
        };
        let (reservation, ownership) =
            crate::mcp::oauth_callback::test_listener::reserve_listener();
        let port = reservation.port();
        let mut config = config("cancel", ORIGIN_URL);
        config.callback_port = Some(port);
        let mut login = Box::pin(mcp_login_with_listener(&http, &config, &ui, reservation));
        tokio::select! {
            result = &mut login => panic!("login settled before cancellation: {result:?}"),
            () = http.entered.notified() => {},
        }
        ui.cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), login)
            .await
            .expect("explicit cancel must not wait for HTTP")
            .unwrap_err();
        assert_eq!(error.to_string(), "Login cancelled");
        assert!(http.dropped.load(std::sync::atomic::Ordering::SeqCst));
        if hang_url == ORIGIN_TOKEN {
            assert!(ownership.is_closed());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn whole_flow_deadline_includes_pending_discovery() {
    let http = HangingHttp {
        scripted: ScriptedHttp::new(vec![]),
        hang_url: ORIGIN_URL,
        entered: tokio::sync::Notify::new(),
        dropped: std::sync::atomic::AtomicBool::new(false),
    };
    let ui = test_ui();
    let config = config("deadline", ORIGIN_URL);
    let mut login = Box::pin(mcp_login(&http, &config, &ui));
    assert!(futures::poll!(login.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(120)).await;
    let error = tokio::time::timeout(Duration::from_secs(1), login)
        .await
        .expect("the whole-flow deadline must settle login")
        .unwrap_err();
    assert_eq!(error.to_string(), "Login timed out");
    assert!(http.dropped.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn pre_cancelled_login_issues_no_requests() {
    let http = ScriptedHttp::new(vec![]);
    let ui = CancelUi {
        cancel: tokio_util::sync::CancellationToken::new(),
        paste: test_ui(),
    };
    ui.cancel.cancel();
    let error = mcp_login(&http, &config("cancel", ORIGIN_URL), &ui)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Login cancelled");
    assert!(http.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn whole_flow_deadline_interrupts_token_exchange() {
    let http = HangingHttp {
        scripted: ScriptedHttp::new(vec![
            (ORIGIN_URL, 404, None, ""),
            (
                "https://srv.test/.well-known/oauth-protected-resource/mcp",
                404,
                None,
                "",
            ),
            (ORIGIN_META_URL, 200, None, &json_response(&origin_meta())),
        ]),
        hang_url: ORIGIN_TOKEN,
        entered: tokio::sync::Notify::new(),
        dropped: std::sync::atomic::AtomicBool::new(false),
    };
    let (reservation, ownership) = crate::mcp::oauth_callback::test_listener::reserve_listener();
    let port = reservation.port();
    let mut config = config("deadline", ORIGIN_URL);
    config.client_id = Some("client".to_string());
    config.callback_port = Some(port);
    let ui = test_ui();
    let mut login = Box::pin(mcp_login_with_listener(&http, &config, &ui, reservation));
    tokio::select! {
        result = &mut login => panic!("login settled before exchange: {result:?}"),
        () = http.entered.notified() => {},
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(120)).await;
    let error = tokio::time::timeout(Duration::from_secs(1), login)
        .await
        .expect("the whole-flow deadline must settle exchange")
        .unwrap_err();
    tokio::time::resume();
    assert_eq!(error.to_string(), "Login timed out");
    assert!(http.dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        ownership.is_closed(),
        "the whole-flow deadline closes actual owned callback sockets"
    );
}

#[tokio::test(start_paused = true)]
async fn whole_flow_deadline_closes_pending_callback_listener() {
    struct PendingUi(Mutex<String>);
    impl McpLoginUi for PendingUi {
        fn on_progress(&self, _message: &str) {}
        fn on_auth(&self, url: &str, _instructions: &str) {
            *self.0.lock().unwrap() = url.to_string();
        }
        fn on_prompt(
            &self,
            _message: &str,
            _placeholder: &str,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
            Box::pin(std::future::pending())
        }
        fn on_manual_code_input(
            &self,
        ) -> Option<Pin<Box<dyn Future<Output = Option<String>> + Send>>> {
            None
        }
    }
    let http = ScriptedHttp::new(vec![
        (ORIGIN_URL, 404, None, ""),
        (
            "https://srv.test/.well-known/oauth-protected-resource/mcp",
            404,
            None,
            "",
        ),
        (ORIGIN_META_URL, 200, None, &json_response(&origin_meta())),
    ]);
    let (reservation, ownership) = crate::mcp::oauth_callback::test_listener::reserve_listener();
    let port = reservation.port();
    let mut config = config("deadline", ORIGIN_URL);
    config.client_id = Some("client".to_string());
    config.callback_port = Some(port);
    let ui = PendingUi(Mutex::default());
    let mut login = Box::pin(mcp_login_with_listener(&http, &config, &ui, reservation));
    assert!(futures::poll!(login.as_mut()).is_pending());
    assert!(!ui.0.lock().unwrap().is_empty());
    assert!(
        ownership.is_listening(),
        "actual callback owns the reserved listener before deadline"
    );
    tokio::time::advance(Duration::from_secs(120)).await;
    let error = tokio::time::timeout(Duration::from_secs(1), login)
        .await
        .expect("deadline must end the callback wait")
        .unwrap_err();
    assert_eq!(error.to_string(), "Login timed out");
    assert!(
        ownership.is_closed(),
        "the whole-flow deadline closes actual owned callback sockets"
    );
}

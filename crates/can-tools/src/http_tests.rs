use super::*;
use crate::testkit::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Server, der jede Anfrage mit der nächsten vorbereiteten Antwort bedient.
async fn serve(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    format!("http://localhost:{}", address.port())
}

fn ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn redirect(location: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    )
}

async fn call(tool: &HttpFetch, url: &str) -> Result<String, String> {
    let dir = TempDir::new();
    match tool.call(json!({"url": url}), &dir.ctx()).await {
        Ok(output) => Ok(output.as_text()),
        Err(ToolError::ForModel(message)) => Err(message),
        Err(other) => panic!("{other:?}"),
    }
}

fn local() -> HttpFetch {
    HttpFetch::new(vec!["localhost".into()]).allow_private_networks(true)
}

#[test]
fn private_and_local_ranges_are_blocked() {
    for blocked in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "0.0.0.0",
        "100.64.0.1",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fc00::1",
        "fd12::1",
        "fe80::1",
        "ff02::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
    ] {
        assert!(is_blocked(blocked.parse().unwrap()), "{blocked}");
    }
    for public in [
        "93.184.216.34",
        "8.8.8.8",
        "100.128.0.1",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
    ] {
        assert!(!is_blocked(public.parse().unwrap()), "{public}");
    }
}

#[test]
fn spec_is_external() {
    let tool = HttpFetch::new(vec![]);
    assert_eq!(
        (tool.spec().name.as_str(), tool.spec().effect),
        ("http_fetch", Effect::External)
    );
}

#[tokio::test]
async fn fetches_from_an_allowed_host() {
    let base = serve(vec![ok("hallo welt")]).await;
    assert_eq!(
        call(&local(), &format!("{base}/seite")).await.unwrap(),
        "HTTP 200\n\nhallo welt"
    );
}

#[tokio::test]
async fn hosts_outside_the_allowlist_and_bad_urls_are_refused() {
    let tool = HttpFetch::new(vec!["*.example.com".into()]);
    assert!(
        call(&tool, "https://evil.test/")
            .await
            .unwrap_err()
            .contains("Freigabeliste")
    );
    assert!(
        call(&tool, "ftp://a.example.com/")
            .await
            .unwrap_err()
            .contains("Nur http und https")
    );
    assert!(
        call(&tool, "kein url")
            .await
            .unwrap_err()
            .contains("Ungültige Adresse")
    );
    assert!(
        call(&HttpFetch::new(vec![]), "https://a.example.com/")
            .await
            .unwrap_err()
            .contains("Freigabeliste")
    );
    let dir = TempDir::new();
    assert!(
        matches!(tool.call(json!({}), &dir.ctx()).await, Err(ToolError::ForModel(m)) if m.contains("`url`"))
    );
}

#[tokio::test]
async fn loopback_is_blocked_even_when_the_name_is_allowed() {
    let base = serve(vec![]).await;
    let tool = HttpFetch::new(vec!["localhost".into(), "127.0.0.1".into()]);
    assert!(
        call(&tool, &base)
            .await
            .unwrap_err()
            .contains("private oder lokale Adresse")
    );
    let by_address = base.replace("localhost", "127.0.0.1");
    assert!(
        call(&tool, &by_address)
            .await
            .unwrap_err()
            .contains("private oder lokale Adresse")
    );
}

#[tokio::test]
async fn redirects_are_followed_but_checked_again() {
    let target = serve(vec![ok("am ziel")]).await;
    let start = serve(vec![redirect(&format!("{target}/neu"))]).await;
    assert_eq!(call(&local(), &start).await.unwrap(), "HTTP 200\n\nam ziel");

    // Die Weiterleitung zeigt auf einen Host außerhalb der Freigabeliste.
    let start = serve(vec![redirect("http://intern.test/geheim")]).await;
    assert!(
        call(&local(), &start)
            .await
            .unwrap_err()
            .contains("intern.test")
    );
}

#[tokio::test]
async fn endless_redirects_stop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let location = format!("{base}/nochmal");
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            socket
                .write_all(redirect(&location).as_bytes())
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    assert!(
        call(&local(), &base)
            .await
            .unwrap_err()
            .contains("Weiterleitungen")
    );
}

#[tokio::test]
async fn large_bodies_are_cut_and_cancel_stops_the_call() {
    let base = serve(vec![ok(&"x".repeat(5_000))]).await;
    let mut tool = local();
    tool.max_bytes = 100;
    assert_eq!(
        call(&tool, &base).await.unwrap().len(),
        "HTTP 200\n\n".len() + 100
    );

    let dir = TempDir::new();
    let cx = dir.ctx();
    cx.cancel.cancel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let cancelled = local().call(json!({"url": silent}), &cx).await;
    assert!(matches!(cancelled, Err(ToolError::ForModel(m)) if m == "Abgebrochen."));
}

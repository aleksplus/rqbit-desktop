//! HTTP transport for hooks (reqwest).

use std::time::Duration;

use crate::config::{HookConfig, HttpMethod};

use super::HookEvent;

/// Send `payload` to `hook` for `event` and return the parsed JSON response
/// body, if any. `POST` sends the payload as a JSON body; `GET` sends it
/// urlencoded in a `payload` query parameter.
pub(crate) async fn send(
    client: &reqwest::Client,
    hook: &HookConfig,
    event: HookEvent,
    payload: &serde_json::Value,
) -> anyhow::Result<Option<serde_json::Value>> {
    let timeout = Duration::from_secs(hook.timeout_secs.max(1));
    let url = reqwest::Url::parse(&hook.url)?;

    let request = match hook.method {
        HttpMethod::Post => client
            .post(url)
            .timeout(timeout)
            .json(payload)
            .header("X-Rqbit-Event", event.as_str()),
        HttpMethod::Get => {
            let payload_str = serde_json::to_string(payload)?;
            client
                .get(url)
                .timeout(timeout)
                .query(&[("payload", payload_str)])
                .header("X-Rqbit-Event", event.as_str())
        }
    };
    let request = hook
        .headers
        .iter()
        .fold(request, |req, (k, v)| req.header(k, v));

    let response = request.send().await?.error_for_status()?;
    let body = response.bytes().await?;
    if body.is_empty() {
        return Ok(None);
    }
    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| anyhow::anyhow!("hook response is not valid JSON: {e}"))?;
    Ok(Some(parsed))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Minimal one-shot HTTP server for tests: accepts a single request,
    //! returns the raw bytes, and replies with `respond_body`.

    pub(crate) async fn serve_once(
        listener: tokio::net::TcpListener,
        respond_body: &'static str,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        // Read until headers + body are complete (requests are small).
        loop {
            let n = sock.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let s = String::from_utf8_lossy(&buf);
            if let Some(header_end) = s.find("\r\n\r\n") {
                let content_length = s
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                    })
                    .flatten()
                    .unwrap_or(0);
                if buf.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        let raw = String::from_utf8_lossy(&buf).into_owned();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            respond_body.len(),
            respond_body
        );
        sock.write_all(response.as_bytes()).await.unwrap();
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookEvent;
    use crate::hooks::http::testutil::serve_once;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn get_and_post_hit_test_server() {
        let payload = serde_json::json!({"event": "torrent_added", "value": 42});

        // POST
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let received = received.clone();
            async move {
                let raw = serve_once(listener, r#"{"ok": true}"#).await;
                let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
                let request_line = head.lines().next().unwrap_or_default().to_string();
                received
                    .lock()
                    .await
                    .push((request_line, head.to_string(), body.to_string()));
            }
        };

        let hook = HookConfig {
            name: "test".into(),
            enabled: true,
            events: vec![HookEvent::TorrentAdded],
            method: HttpMethod::Post,
            url: format!("http://{addr}/hook"),
            timeout_secs: 5,
            headers: [("X-Token".to_string(), "secret".to_string())].into(),
            poll_secs: 10,
        };
        let client = reqwest::Client::new();
        let (result, _) = tokio::join!(
            send(&client, &hook, HookEvent::TorrentAdded, &payload),
            server
        );
        assert_eq!(result.unwrap(), Some(serde_json::json!({"ok": true})));
        let (line, head, body) = received.lock().await.remove(0);
        assert!(line.starts_with("POST /hook "));
        assert!(head.to_lowercase().contains("x-token: secret"));
        assert_eq!(body, r#"{"event":"torrent_added","value":42}"#);

        // GET
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let received = received.clone();
            async move {
                let raw = serve_once(listener, "").await;
                let request_line = raw
                    .split_once("\r\n\r\n")
                    .map(|(h, _)| h.lines().next().unwrap_or_default().to_string())
                    .unwrap_or_default();
                received
                    .lock()
                    .await
                    .push((request_line, String::new(), String::new()));
            }
        };

        let hook = HookConfig {
            name: "test".into(),
            enabled: true,
            events: vec![HookEvent::TorrentAdded],
            method: HttpMethod::Get,
            url: format!("http://{addr}/hook"),
            timeout_secs: 5,
            headers: Default::default(),
            poll_secs: 10,
        };
        let client = reqwest::Client::new();
        let (result, _) = tokio::join!(
            send(&client, &hook, HookEvent::TorrentAdded, &payload),
            server
        );
        assert_eq!(result.unwrap(), None); // empty body → None
        let (line, _, _) = received.lock().await.remove(0);
        assert!(line.starts_with("GET /hook?payload="), "got: {line}");
    }
}

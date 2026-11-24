//! The HTTP calls the list services need: JSON back, bearer auth, form
//! bodies, and 401s told apart from other failures. Error messages carry
//! the server's short message only, never request headers or bodies, so a
//! token can't end up in a log line or toast.

use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use serde_json::Value;
use ureq::Body;
use ureq::http::Response;

const TIMEOUT: Duration = Duration::from_secs(20);
const USER_AGENT: &str = "ani-gui/1.0";
/// Longest server message kept in an error.
const MAX_MESSAGE: usize = 200;

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .build()
        .into()
});

#[derive(Debug)]
pub enum HttpError {
    /// 401: the token is missing, expired or revoked.
    Unauthorized,
    /// Any other non-2xx status, with the server's message if it gave one.
    Status(u16, String),
    /// Couldn't connect, timed out, or the reply wasn't JSON.
    Transport(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Unauthorized => f.write_str("not signed in (HTTP 401)"),
            HttpError::Status(code, msg) if msg.is_empty() => write!(f, "HTTP {code}"),
            HttpError::Status(code, msg) => write!(f, "HTTP {code}: {msg}"),
            HttpError::Transport(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for HttpError {}

fn auth(bearer: Option<&str>) -> Option<String> {
    bearer.map(|token| format!("Bearer {token}"))
}

pub fn get(url: &str, bearer: Option<&str>) -> Result<Value, HttpError> {
    let mut req = AGENT.get(url).header("Accept", "application/json");
    if let Some(value) = auth(bearer) {
        req = req.header("Authorization", value);
    }
    finish(req.call())
}

pub fn post_form(
    url: &str,
    bearer: Option<&str>,
    form: &[(&str, &str)],
) -> Result<Value, HttpError> {
    let mut req = AGENT.post(url).header("Accept", "application/json");
    if let Some(value) = auth(bearer) {
        req = req.header("Authorization", value);
    }
    finish(req.send_form(form.iter().copied()))
}

pub fn patch_form(
    url: &str,
    bearer: Option<&str>,
    form: &[(&str, &str)],
) -> Result<Value, HttpError> {
    let mut req = AGENT.patch(url).header("Accept", "application/json");
    if let Some(value) = auth(bearer) {
        req = req.header("Authorization", value);
    }
    finish(req.send_form(form.iter().copied()))
}

pub fn post_json(url: &str, bearer: Option<&str>, body: &Value) -> Result<Value, HttpError> {
    let mut req = AGENT.post(url).header("Accept", "application/json");
    if let Some(value) = auth(bearer) {
        req = req.header("Authorization", value);
    }
    finish(req.send_json(body))
}

fn finish(sent: Result<Response<Body>, ureq::Error>) -> Result<Value, HttpError> {
    let mut response = sent.map_err(|err| HttpError::Transport(transport_message(&err)))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|err| HttpError::Transport(format!("reading the reply: {err}")))?;
    let json = if text.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str::<Value>(&text)
    };
    if status == 401 {
        return Err(HttpError::Unauthorized);
    }
    if !(200..300).contains(&status) {
        let message = json.as_ref().map(server_message).unwrap_or_default();
        return Err(HttpError::Status(status, message));
    }
    json.map_err(|_| HttpError::Transport("the reply wasn't JSON".into()))
}

/// ureq's own description, which names the host but no headers.
fn transport_message(err: &ureq::Error) -> String {
    match err {
        ureq::Error::Timeout(_) => "the server took too long to answer".into(),
        ureq::Error::HostNotFound => "couldn't find the server (are you online?)".into(),
        ureq::Error::Io(io) => format!("couldn't reach the server: {io}"),
        other => format!("request failed: {other}"),
    }
}

/// The error text a JSON error body carries: OAuth's `error_description`
/// / `error`, MAL's `message`, or GraphQL's first `errors[].message`.
pub fn server_message(v: &Value) -> String {
    let text = [
        &v["error_description"],
        &v["message"],
        &v["errors"][0]["message"],
        &v["error"],
    ]
    .into_iter()
    .find_map(|v| v.as_str().filter(|s| !s.trim().is_empty()))
    .unwrap_or_default();
    clip(text)
}

/// `text` on one line, at most `MAX_MESSAGE` characters, printable only.
fn clip(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_MESSAGE)
        .collect();
    if text.chars().count() > MAX_MESSAGE {
        out.push('…');
    }
    out.trim().to_string()
}

/// A one-shot local HTTP server for tests: answers each request with the
/// next canned (status, body) and records the requests it got.
#[cfg(test)]
pub mod fake {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    #[derive(Clone, Debug, Default)]
    pub struct Seen {
        pub method: String,
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Seen {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    pub struct Server {
        pub base: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl Server {
        pub fn start(replies: Vec<(u16, String)>) -> Server {
            Server::start_with(|_| replies)
        }

        /// Like `start`, with replies that may mention the server's own
        /// base URL.
        pub fn start_with(replies: impl FnOnce(&str) -> Vec<(u16, String)>) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let replies = replies(&base);
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = seen.clone();
            let handle = std::thread::spawn(move || {
                for (status, body) in replies {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let mut parts = line.split_whitespace();
                    let mut req = Seen {
                        method: parts.next().unwrap_or_default().into(),
                        path: parts.next().unwrap_or_default().into(),
                        ..Default::default()
                    };
                    let mut len = 0;
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        if let Some((n, v)) = h.split_once(':') {
                            if n.eq_ignore_ascii_case("content-length") {
                                len = v.trim().parse().unwrap();
                            }
                            req.headers.push((n.trim().into(), v.trim().into()));
                        }
                    }
                    let mut body_bytes = vec![0; len];
                    reader.read_exact(&mut body_bytes).unwrap();
                    req.body = String::from_utf8(body_bytes).unwrap();
                    log.lock().unwrap().push(req);
                    let mut stream = reader.into_inner();
                    write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            Server {
                base,
                seen,
                handle: Some(handle),
            }
        }

        /// Waits for every canned reply to be served, then returns the
        /// requests.
        pub fn finish(mut self) -> Vec<Seen> {
            if let Some(handle) = self.handle.take() {
                handle.join().unwrap();
            }
            self.seen.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::fake::Server;
    use super::*;

    #[test]
    fn statuses_map_to_errors() {
        let server = Server::start(vec![
            (200, r#"{"ok":true}"#.into()),
            (401, r#"{"error":"invalid_token"}"#.into()),
            (
                400,
                r#"{"error":"invalid_grant","message":"bad code"}"#.into(),
            ),
            (500, "<html>oops</html>".into()),
            (204, String::new()),
        ]);
        let url = format!("{}/x", server.base);
        assert_eq!(get(&url, Some("tok")).unwrap(), json!({ "ok": true }));
        assert!(matches!(get(&url, None), Err(HttpError::Unauthorized)));
        match post_form(&url, None, &[("a", "b")]) {
            Err(HttpError::Status(400, msg)) => assert_eq!(msg, "bad code"),
            other => panic!("{other:?}"),
        }
        match get(&url, None) {
            Err(HttpError::Status(500, msg)) => assert_eq!(msg, ""),
            other => panic!("{other:?}"),
        }
        assert_eq!(patch_form(&url, Some("t"), &[]).unwrap(), Value::Null);
        let seen = server.finish();
        assert_eq!(seen[0].header("authorization"), Some("Bearer tok"));
        assert_eq!(seen[1].header("authorization"), None);
        assert_eq!(seen[2].body, "a=b");
        assert_eq!(seen[4].method, "PATCH");
    }

    #[test]
    fn server_message_is_short_and_single_line() {
        assert_eq!(
            server_message(&json!({ "errors": [{ "message": "Invalid token" }] })),
            "Invalid token"
        );
        assert_eq!(
            server_message(
                &json!({ "error": "invalid_grant", "error_description": "expired\ncode" })
            ),
            "expired code"
        );
        let long = "x".repeat(500);
        let msg = server_message(&json!({ "message": long }));
        assert_eq!(msg.chars().count(), MAX_MESSAGE + 1);
        assert_eq!(server_message(&json!([1, 2])), "");
    }
}

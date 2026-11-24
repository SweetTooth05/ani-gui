//! MyAnimeList's OAuth2 sign-in pieces: PKCE verifier and state from the
//! OS CSPRNG, a loopback listener that takes exactly one valid redirect,
//! and the platform's URL opener.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::opener;

/// Fixed so the user can register the exact redirect URI with MAL.
pub const REDIRECT_PORT: u16 = 53682;
pub const REDIRECT_PATH: &str = "/callback";
pub const REDIRECT_URI: &str = "http://localhost:53682/callback";
/// How long the listener waits for the browser.
pub const WAIT_FOR: Duration = Duration::from_secs(5 * 60);

/// RFC 7636's unreserved characters, the only ones a verifier may use.
const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
/// Longest request head read from the browser.
const MAX_REQUEST: usize = 8 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CODE: usize = 2048;

/// `len` characters drawn uniformly from `UNRESERVED` using the OS CSPRNG
/// (rejection sampling keeps every character equally likely).
pub fn random_unreserved(len: usize) -> Result<String> {
    let n = UNRESERVED.len();
    let limit = 256 - 256 % n;
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        getrandom::fill(&mut buf).map_err(|err| anyhow!("no secure random numbers: {err}"))?;
        for &b in &buf {
            if (b as usize) < limit && out.len() < len {
                out.push(UNRESERVED[b as usize % n] as char);
            }
        }
    }
    Ok(out)
}

/// A PKCE code verifier. MAL only supports the `plain` method, so this is
/// also the challenge: 128 characters, the most RFC 7636 allows.
pub fn pkce_verifier() -> Result<String> {
    random_unreserved(128)
}

/// The `state` value tying a redirect to the sign-in that started it.
pub fn new_state() -> Result<String> {
    random_unreserved(32)
}

/// Percent-encodes everything but unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` and `+` in a query component; None for broken escapes or
/// bytes that aren't UTF-8.
fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// What one request to the loopback listener amounts to.
#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    /// The redirect with our state and a code: sign-in succeeded.
    Code(String),
    /// The redirect with our state and an error (the user said no).
    Denied(String),
    /// Anything else on /callback: wrong or missing state, junk, repeats.
    Invalid,
    /// Some other path (the browser asking for /favicon.ico).
    NotFound,
}

/// Classifies a request head (request line and headers). Only a GET of
/// exactly `/callback?…` carrying `expected_state` once counts.
pub fn parse_callback(head: &str, expected_state: &str) -> Callback {
    let Some(line) = head.split("\r\n").next() else {
        return Callback::Invalid;
    };
    let parts: Vec<&str> = line.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Callback::Invalid;
    };
    if !version.starts_with("HTTP/1.") {
        return Callback::Invalid;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != REDIRECT_PATH {
        return Callback::NotFound;
    }
    if method != "GET" {
        return Callback::Invalid;
    }
    let mut state = None;
    let mut code = None;
    let mut error = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(value) = decode(value) else {
            return Callback::Invalid;
        };
        let slot = match key {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        // A repeated parameter is ambiguous: refuse rather than guess.
        if slot.replace(value).is_some() {
            return Callback::Invalid;
        }
    }
    match state {
        Some(state) if same(&state, expected_state) => {}
        _ => return Callback::Invalid,
    }
    match (code, error) {
        (Some(code), None)
            if !code.is_empty()
                && code.len() <= MAX_CODE
                && code.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Callback::Code(code)
        }
        (None, Some(error)) => Callback::Denied(error),
        _ => Callback::Invalid,
    }
}

/// Compares in time independent of where the strings differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Binds the fixed redirect port on loopback only.
pub fn bind() -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).with_context(|| {
        format!(
            "Couldn't listen on port {REDIRECT_PORT} for MyAnimeList's redirect. \
             Is another sign-in (or another program) using it?"
        )
    })
}

/// Serves requests on `listener` until one is our redirect, then returns
/// its code. Gives up at `deadline`, when `cancel` is set, or when the user
/// denies access. Junk requests get an error page and are otherwise ignored.
pub fn wait_for_code(
    listener: &TcpListener,
    state: &str,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<String> {
    listener.set_nonblocking(true)?;
    loop {
        if cancel.load(Ordering::SeqCst) {
            bail!("Sign-in cancelled");
        }
        if Instant::now() >= deadline {
            bail!("Sign-in timed out. Try connecting again.");
        }
        match listener.accept() {
            Ok((stream, _)) => match handle(stream, state) {
                Some(Ok(code)) => return Ok(code),
                Some(Err(err)) => return Err(err),
                None => {}
            },
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("waiting for the browser"),
        }
    }
}

/// Answers one connection. Some(..) ends the wait.
fn handle(mut stream: TcpStream, state: &str) -> Option<Result<String>> {
    let head = read_head(&mut stream)?;
    let callback = parse_callback(&head, state);
    let (status, body) = match &callback {
        Callback::Code(_) => ("200 OK", PAGE_DONE),
        Callback::Denied(_) => ("200 OK", PAGE_DENIED),
        Callback::Invalid => ("400 Bad Request", PAGE_INVALID),
        Callback::NotFound => ("404 Not Found", PAGE_NOT_FOUND),
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
    match callback {
        Callback::Code(code) => Some(Ok(code)),
        Callback::Denied(_) => Some(Err(anyhow!(
            "MyAnimeList sign-in was cancelled in the browser"
        ))),
        _ => None,
    }
}

/// Reads until the blank line ending the request head; None for clients
/// that are slow, send too much, or aren't sending text.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() >= MAX_REQUEST {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8(buf).ok()
}

// Static pages: nothing from the request is ever echoed back.
const PAGE_DONE: &str = "<!doctype html><meta charset=utf-8><title>ani-gui</title>\
    <body style=\"font-family:sans-serif;background:#0e1018;color:#eef0f6;padding:48px\">\
    <h1>You're signed in</h1><p>You can close this tab and go back to ani-gui.</p>";
const PAGE_DENIED: &str = "<!doctype html><meta charset=utf-8><title>ani-gui</title>\
    <body style=\"font-family:sans-serif;background:#0e1018;color:#eef0f6;padding:48px\">\
    <h1>Sign-in cancelled</h1><p>You can close this tab. Nothing was connected.</p>";
const PAGE_INVALID: &str = "<!doctype html><meta charset=utf-8><title>ani-gui</title>\
    <body style=\"font-family:sans-serif;background:#0e1018;color:#eef0f6;padding:48px\">\
    <h1>This sign-in link isn't valid</h1><p>Start again from ani-gui's Settings.</p>";
const PAGE_NOT_FOUND: &str = "<!doctype html><meta charset=utf-8><title>Not found</title>";

/// Opens an https URL with the platform's opener (see [`opener::open`]).
pub fn open_browser(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("refusing to open a non-https URL");
    }
    // The browser's exit status means nothing here.
    let _exited = opener::open(url.as_ref())
        .with_context(|| format!("Couldn't run {} to open your browser", opener::PROGRAM))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn verifier_and_state_are_unreserved_and_random() {
        let v = pkce_verifier().unwrap();
        assert_eq!(v.len(), 128);
        assert!(v.bytes().all(|b| UNRESERVED.contains(&b)));
        assert_ne!(v, pkce_verifier().unwrap());
        let s = new_state().unwrap();
        assert_eq!(s.len(), 32);
        assert_ne!(s, new_state().unwrap());
        // Every character of the alphabet shows up across a large sample.
        let sample = random_unreserved(20_000).unwrap();
        for &c in UNRESERVED {
            assert!(sample.as_bytes().contains(&c), "{}", c as char);
        }
    }

    #[test]
    fn encode_and_decode() {
        assert_eq!(encode("a b&c=d/é~"), "a%20b%26c%3Dd%2F%C3%A9~");
        assert_eq!(decode("a%20b+c%2F").as_deref(), Some("a b c/"));
        assert_eq!(decode("%zz"), None);
        assert_eq!(decode("%4"), None);
        assert_eq!(decode("%FF"), None);
    }

    fn get(target: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: localhost:53682\r\n\r\n")
    }

    #[test]
    fn callback_accepts_our_redirect() {
        assert_eq!(
            parse_callback(&get("/callback?code=def502%2Babc&state=S1"), "S1"),
            Callback::Code("def502+abc".into())
        );
        assert_eq!(
            parse_callback(&get("/callback?state=S1&code=xyz&extra=1"), "S1"),
            Callback::Code("xyz".into())
        );
        assert_eq!(
            parse_callback(&get("/callback?error=access_denied&state=S1"), "S1"),
            Callback::Denied("access_denied".into())
        );
    }

    #[test]
    fn callback_rejects_garbage_and_forgeries() {
        let invalid = [
            get("/callback?code=xyz&state=WRONG"),
            get("/callback?code=xyz"),
            get("/callback?code=xyz&state="),
            get("/callback?code=xyz&state=S1&state=S1"),
            get("/callback?code=a&code=b&state=S1"),
            get("/callback?code=&state=S1"),
            get("/callback?code=x%20y&state=S1"),
            get("/callback?state=S1"),
            get("/callback?code=x&error=access_denied&state=S1"),
            get("/callback?code=%zz&state=S1"),
            get("/callback?error=denied&state=S2"),
            get(&format!(
                "/callback?code={}&state=S1",
                "a".repeat(MAX_CODE + 1)
            )),
            "POST /callback?code=xyz&state=S1 HTTP/1.1\r\n\r\n".into(),
            "GET /callback?code=xyz&state=S1\r\n\r\n".into(),
            "GET /callback?code=xyz&state=S1 SPDY/3\r\n\r\n".into(),
            "GET  /callback?code=xyz&state=S1 HTTP/1.1\r\n\r\n".into(),
            "\u{0}\u{1}garbage".into(),
            String::new(),
        ];
        for head in &invalid {
            assert_eq!(parse_callback(head, "S1"), Callback::Invalid, "{head:?}");
        }
        for path in [
            "/favicon.ico",
            "/",
            "/callback/x?code=1&state=S1",
            "/callbackx?state=S1",
        ] {
            assert_eq!(
                parse_callback(&get(path), "S1"),
                Callback::NotFound,
                "{path}"
            );
        }
    }

    /// Sends `request` to the listener and returns the response.
    fn send(port: u16, request: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn listener_ignores_junk_until_the_real_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = AtomicBool::new(false);
        let client = thread::spawn(move || {
            let junk = send(port, &get("/callback?code=evil&state=WRONG"));
            let icon = send(port, &get("/favicon.ico"));
            // A client that connects and says nothing is dropped after the
            // read timeout without ending the wait.
            let silent = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let ok = send(port, &get("/callback?code=good&state=S1"));
            drop(silent);
            (junk, icon, ok)
        });
        let code = wait_for_code(
            &listener,
            "S1",
            Instant::now() + Duration::from_secs(20),
            &cancel,
        )
        .unwrap();
        assert_eq!(code, "good");
        let (junk, icon, ok) = client.join().unwrap();
        assert!(junk.starts_with("HTTP/1.1 400"), "{junk}");
        assert!(!junk.contains("evil"));
        assert!(icon.starts_with("HTTP/1.1 404"), "{icon}");
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("You can close this tab"));
    }

    #[test]
    fn listener_reports_denial_timeout_and_cancel() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let client =
            thread::spawn(move || send(port, &get("/callback?error=access_denied&state=S1")));
        let cancel = AtomicBool::new(false);
        let err = wait_for_code(
            &listener,
            "S1",
            Instant::now() + Duration::from_secs(20),
            &cancel,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
        assert!(client.join().unwrap().contains("Sign-in cancelled"));

        let err = wait_for_code(&listener, "S1", Instant::now(), &cancel).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");

        let cancel = Arc::new(AtomicBool::new(true));
        let err = wait_for_code(&listener, "S1", Instant::now() + WAIT_FOR, &cancel).unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");
    }

    #[test]
    fn open_browser_only_opens_https() {
        assert!(open_browser("http://example.com").is_err());
        assert!(open_browser("file:///etc/passwd").is_err());
        assert!(open_browser("--help").is_err());
    }
}

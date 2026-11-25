use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = "ani-gui/1.0";

static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .max_idle_connections(32)
        .max_idle_connections_per_host(12)
        .user_agent(USER_AGENT)
        .build()
        .into()
});

/// POSTs `body` as JSON and decodes the JSON response. Non-2xx statuses are
/// errors. A `User-Agent` entry in `headers` replaces the default one.
pub fn post_json(url: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value> {
    let mut req = AGENT.post(url).header("Accept", "application/json");
    for &(name, value) in headers {
        req = req.header(name, value);
    }
    req.send_json(body)
        .with_context(|| format!("POST {url}"))?
        .body_mut()
        .read_json()
        .with_context(|| format!("decoding JSON from {url}"))
}

pub fn get_bytes(url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut req = AGENT.get(url);
    for &(name, value) in headers {
        req = req.header(name, value);
    }
    req.call()
        .with_context(|| format!("GET {url}"))?
        .body_mut()
        .read_to_vec()
        .with_context(|| format!("reading body of {url}"))
}

use std::time::Duration;

use reqwest::{
    blocking::Client,
    header::{HeaderName, HeaderValue},
    redirect::Policy,
};

use crate::event_graph::EventEnvelope;

use super::{
    model::{HttpActionConfig, HttpMethod},
    secrets::SecretStore,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOutcome {
    pub status_code: u16,
}

pub fn execute(
    config: &HttpActionConfig,
    event: Option<&EventEnvelope>,
    secrets: &SecretStore,
    action_timeout_ms: u64,
) -> std::result::Result<HttpOutcome, String> {
    config.validate()?;
    let timeout = Duration::from_millis(config.timeout_ms.min(action_timeout_ms));
    let client = Client::builder()
        .timeout(timeout)
        .redirect(Policy::none())
        // TLS verification remains enabled; no dangerous certificate option is
        // exposed by the Action configuration.
        .build()
        .map_err(|_| "ACTION_HTTP_FAILED: HTTP client initialization failed".to_string())?;
    let method = match config.method {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
        HttpMethod::Patch => reqwest::Method::PATCH,
        HttpMethod::Delete => reqwest::Method::DELETE,
    };
    let mut request = client.request(method, &config.url);
    for header in &config.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| "ACTION_HTTP_FAILED: invalid header name".to_string())?;
        let value = match (header.value.as_ref(), header.secret_id) {
            (Some(value), None) => value.clone(),
            (None, Some(secret_id)) => secrets
                .get_persistent(secret_id)
                .map_err(|_| {
                    "ACTION_HTTP_FAILED: header secret storage is unavailable".to_string()
                })?
                .ok_or_else(|| {
                    "ACTION_HTTP_FAILED: required header secret is missing".to_string()
                })?,
            _ => return Err("ACTION_HTTP_FAILED: invalid header configuration".into()),
        };
        let value = HeaderValue::from_str(&value)
            .map_err(|_| "ACTION_HTTP_FAILED: invalid header value".to_string())?;
        request = request.header(name, value);
    }
    if let Some(template) = &config.body_template {
        request = request.body(render_template(template, event));
    }
    let response = request.send().map_err(|error| {
        if error.is_timeout() {
            "ACTION_HTTP_TIMEOUT: request timed out".to_string()
        } else {
            "ACTION_HTTP_FAILED: request failed".to_string()
        }
    })?;
    let status_code = response.status().as_u16();
    if !response.status().is_success() {
        return Err(format!(
            "ACTION_HTTP_FAILED: server returned status {status_code}"
        ));
    }
    Ok(HttpOutcome { status_code })
}

fn render_template(template: &str, event: Option<&EventEnvelope>) -> String {
    let Some(event) = event else {
        return template
            .replace("{{event_type}}", "test.action")
            .replace("{{payload}}", "null");
    };
    template
        .replace("{{event_type}}", &event.event_type)
        .replace("{{payload}}", &event.payload.to_string())
}

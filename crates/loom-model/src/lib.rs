//! OpenAI-compatible provider boundary. Credentials never enter events or errors.
use anyhow::{Result, anyhow, ensure};
use loom_proto::{LlmArgs, LlmResult};
use loom_store::Store;
use serde_json::json;
use std::time::Duration;

pub const MODEL_PROVIDER: &str = "builtin:model";

/// Intentionally does not implement Debug: configuration contains credentials.
pub struct Config {
    pub url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
}
#[derive(Clone)]
pub struct Model {
    store: Store,
    provider: Option<Provider>,
}
#[derive(Clone)]
struct Provider {
    client: reqwest::Client,
    url: reqwest::Url,
    authorization: Option<reqwest::header::HeaderValue>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelState {
    pub id: String,
    pub configured: bool,
    pub requests: u64,
    pub completed: u64,
    pub failed: u64,
    pub last_seq: i64,
}
impl Model {
    /// Read persisted provider events; pending calls remain visible after restart.
    pub fn state(&self) -> Result<ModelState> {
        let mut state = ModelState {
            id: MODEL_PROVIDER.into(),
            configured: self.provider.is_some(),
            requests: 0,
            completed: 0,
            failed: 0,
            last_seq: 0,
        };
        let mut after = 0;
        loop {
            let events = self.store.definition_events(after, 1000)?;
            if events.is_empty() {
                break;
            }
            for event in events {
                after = event.seq;
                if event.event["model_provider"] != MODEL_PROVIDER {
                    continue;
                }
                state.last_seq = event.seq;
                match event.event["type"].as_str() {
                    Some("model.request") => state.requests += 1,
                    Some("model.result") => state.completed += 1,
                    Some("model.error") => state.failed += 1,
                    _ => {}
                }
            }
        }
        Ok(state)
    }
    pub fn list(&self) -> Result<Vec<ModelState>> {
        Ok(vec![self.state()?])
    }
    pub fn from_env(store: Store) -> Result<Self> {
        let url = std::env::var("LOOM_LLM_URL").ok();
        let api_key = std::env::var("LOOM_LLM_API_KEY").ok();
        let config = url
            .or_else(|| {
                api_key
                    .as_ref()
                    .map(|_| "https://api.openai.com/v1/chat/completions".into())
            })
            .map(|url| Config {
                url,
                api_key,
                timeout: Duration::from_secs(120),
            });
        Self::new(store, config)
    }
    pub fn new(store: Store, config: Option<Config>) -> Result<Self> {
        let provider = config
            .map(|config| -> Result<Provider> {
                let url = reqwest::Url::parse(&config.url)
                    .map_err(|_| anyhow!("invalid model endpoint URL"))?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none(),
                    "model URL must be HTTP(S) without credentials, query, or fragment"
                );
                let authorization = config
                    .api_key
                    .map(|key| {
                        let mut value =
                            reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                                .map_err(|_| anyhow!("invalid model API key header"))?;
                        value.set_sensitive(true);
                        Ok::<_, anyhow::Error>(value)
                    })
                    .transpose()?;
                let client = reqwest::Client::builder()
                    .timeout(config.timeout)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|_| anyhow!("could not initialize model HTTP client"))?;
                Ok(Provider {
                    client,
                    url,
                    authorization,
                })
            })
            .transpose()?;
        Ok(Self { store, provider })
    }
    pub async fn complete(&self, args: LlmArgs) -> Result<LlmResult> {
        let provider = self
            .provider
            .as_ref()
            .ok_or_else(|| anyhow!("llm unavailable: set LOOM_LLM_URL or LOOM_LLM_API_KEY"))?;
        ensure!(
            !args.model.trim().is_empty() && !args.messages.is_empty(),
            "model and messages are required"
        );
        ensure!(args.max_tokens != Some(0), "max_tokens must be positive");
        ensure!(
            args.temperature
                .is_none_or(|value| value.is_finite() && (0.0..=2.0).contains(&value)),
            "temperature must be between 0 and 2"
        );
        let request_seq = self.store.record_definition_event(
            &json!({"model_provider": MODEL_PROVIDER, "type":"model.request", "args":args}),
        )?;
        let result = provider.complete(&args).await;
        match &result {
            Ok(output) => {
                self.store.record_definition_event(&json!({"model_provider": MODEL_PROVIDER, "type":"model.result","request_seq":request_seq,"result":output}))?;
            }
            Err(error) => {
                self.store.record_definition_event(&json!({"model_provider": MODEL_PROVIDER, "type":"model.error","request_seq":request_seq,"error":error.to_string()}))?;
            }
        }
        result
    }
}
impl Provider {
    async fn complete(&self, args: &LlmArgs) -> Result<LlmResult> {
        let mut request = self.client.post(self.url.clone()).json(args);
        if let Some(authorization) = &self.authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization.clone());
        }
        let mut response = request.send().await.map_err(transport_error)?;
        ensure!(
            response.status().is_success(),
            "model provider HTTP {}",
            response.status().as_u16()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= 1024 * 1024,
                "model response exceeds 1 MiB"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("model provider returned invalid completion JSON"))
    }
}
fn transport_error(error: reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow!("model provider request timed out")
    } else {
        anyhow!("model provider transport failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn args() -> LlmArgs {
        serde_json::from_value(json!({"model":"fixture-model","messages":[{"role":"user","content":"hello"}],"max_tokens":7,"temperature":0.5})).unwrap()
    }
    struct Fixture {
        url: String,
        received: tokio::task::JoinHandle<String>,
    }
    async fn fixture(status: u16, body: &'static str, delay: Duration) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let received = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut stream = accepted.0;
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            tokio::time::sleep(delay).await;
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            String::from_utf8(bytes).unwrap()
        });
        Fixture { url, received }
    }
    #[tokio::test]
    async fn posts_native_completion_and_records_result() {
        let fixture = fixture(200, r#"{"id":"reply","model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"world"},"finish_reason":"stop"}],"usage":{"total_tokens":4}}"#, Duration::ZERO).await;
        let store = Store::memory().unwrap();
        let model = Model::new(
            store.clone(),
            Some(Config {
                url: fixture.url,
                api_key: Some("fixture-secret".into()),
                timeout: Duration::from_secs(2),
            }),
        )
        .unwrap();
        assert_eq!(
            model.complete(args()).await.unwrap().choices[0]
                .message
                .content,
            "world"
        );
        let request = fixture.received.await.unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(request.contains("authorization: Bearer fixture-secret\r\n"));
        let body: serde_json::Value =
            serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["max_tokens"], 7);
        assert_eq!(body["messages"][0]["content"], "hello");
        let state = model.state().unwrap();
        assert_eq!(state.requests, 1);
        assert_eq!(state.completed, 1);
        assert_eq!(state.failed, 0);
        let restored = Model::new(store.clone(), None).unwrap().state().unwrap();
        assert_eq!(restored.completed, 1);
        let events = store.definition_events(0, 100).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event["type"] == "model.result")
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("fixture-secret")
        );
    }
    #[tokio::test]
    async fn failures_are_bounded_and_do_not_echo_provider_secrets() {
        for status in [401, 429, 500] {
            let fixture = fixture(status, "secret provider error", Duration::ZERO).await;
            let model = Model::new(
                Store::memory().unwrap(),
                Some(Config {
                    url: fixture.url,
                    api_key: None,
                    timeout: Duration::from_secs(2),
                }),
            )
            .unwrap();
            assert_eq!(
                model.complete(args()).await.unwrap_err().to_string(),
                format!("model provider HTTP {status}")
            );
            fixture.received.await.unwrap();
        }
        let fixture = fixture(200, "{}", Duration::from_millis(100)).await;
        let model = Model::new(
            Store::memory().unwrap(),
            Some(Config {
                url: fixture.url,
                api_key: None,
                timeout: Duration::from_millis(10),
            }),
        )
        .unwrap();
        assert_eq!(
            model.complete(args()).await.unwrap_err().to_string(),
            "model provider request timed out"
        );
        fixture.received.await.unwrap();
        let model = Model::new(Store::memory().unwrap(), None).unwrap();
        assert!(
            model
                .complete(args())
                .await
                .unwrap_err()
                .to_string()
                .contains("llm unavailable")
        );
    }
}

//! Experimental Cloud Code Assist adapter. Protocol reference:
//! NoeFabris/opencode-antigravity-auth (MIT), src/antigravity/oauth.ts and
//! src/plugin/request.ts. No proxy process, account rotation or quota retry.
use crate::accounts::{Status, save_private};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use mio_core::{
    http_client::{
        self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
        framing::SseFramer,
    },
    providers::gemini::{Gemini, GeminiConfig},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const REDIRECT: &str = "http://localhost:51121/oauth-callback";
const TOKEN: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://cloudcode-pa.googleapis.com";
const IMPORTED_API: &str = "https://daily-cloudcode-pa.googleapis.com";
const USER_AGENT: &str = "antigravity/cli/1.3.0 (aidev_client; os_type=linux; arch=amd64; cl=994000447; auth_method=consumer)";

#[derive(Deserialize, Serialize)]
struct Credential {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    project: String,
}

pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone)]
pub struct Antigravity(Arc<Account>);
struct Account {
    path: PathBuf,
    oauth: OAuthConfig,
    imported: bool,
    token_endpoint: String,
    api_endpoint: String,
    status: Mutex<Status>,
    operation: tokio::sync::Mutex<()>,
    login: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn nonce() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "无法创建 Google 连接".into())
}
async fn json_response(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(network_error)?;
    if !response.status().is_success() {
        return Err(format!(
            "Google 请求返回 HTTP {}，请检查账号授权与接口可用性",
            response.status().as_u16()
        ));
    }
    response
        .json()
        .await
        .map_err(|_| "Google 返回了无效数据".into())
}
fn network_error(error: reqwest::Error) -> String {
    use std::error::Error;
    let mut source = error.source();
    let mut certificate = false;
    let mut proxy = false;
    while let Some(cause) = source {
        let text = cause.to_string().to_lowercase();
        certificate |= text.contains("certificate") || text.contains("unknownissuer");
        proxy |= text.contains("proxy") || text.contains("tunnel");
        source = cause.source();
    }
    let reason = if certificate {
        "TLS certificate validation"
    } else if proxy {
        "proxy connection"
    } else if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection"
    } else {
        "request transport"
    };
    format!("Google network request failed ({reason}); check network/proxy configuration")
}
fn required(value: &Value, key: &str) -> Result<String, String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Google 响应缺少 {key}"))
}

impl Antigravity {
    pub fn new(directory: PathBuf, oauth: OAuthConfig) -> std::io::Result<Self> {
        if oauth.client_id.trim().is_empty() || oauth.client_secret.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Google OAuth client_id and client_secret must not be empty",
            ));
        }
        let path = directory.join("antigravity.json");
        let connected = path.try_exists()?;
        Ok(Self(Arc::new(Account {
            path,
            oauth,
            imported: false,
            token_endpoint: TOKEN.into(),
            api_endpoint: API.into(),
            status: Mutex::new(Status {
                connected,
                ..Status::default()
            }),
            operation: tokio::sync::Mutex::new(()),
            login: tokio::sync::Mutex::new(None),
        })))
    }
    /// Load a native profile imported from an existing local OAuth login.
    pub fn from_profile(directory: PathBuf) -> Result<Self, String> {
        let profile = crate::profile::load(&directory)?;
        let mut service = Self::new(
            directory.clone(),
            OAuthConfig {
                client_id: profile.client_id,
                client_secret: profile.client_secret,
            },
        )
        .map_err(|_| "Cannot initialize Antigravity account")?;
        let account = Arc::get_mut(&mut service.0).expect("new account is uniquely owned");
        account.path = directory.join(crate::profile::PROFILE_FILE);
        account.imported = true;
        account.api_endpoint = IMPORTED_API.into();
        account.status.lock().unwrap().connected = true;
        Ok(service)
    }
    pub async fn client(&self, model: &str) -> Result<Gemini, String> {
        self.client_with_thinking(model, None).await
    }
    pub async fn client_with_thinking(
        &self,
        model: &str,
        thinking_override: Option<&str>,
    ) -> Result<Gemini, String> {
        let _guard = self.0.operation.lock().await;
        let mut stored: Value = serde_json::from_slice(
            &std::fs::read(&self.0.path).map_err(|_| "请先登录 Google Antigravity")?,
        )
        .map_err(|_| "Google 授权数据损坏，请重新登录")?;
        let mut credential: Credential = serde_json::from_value(stored.clone())
            .map_err(|_| "Google 授权数据损坏，请重新登录")?;
        let http = http()?;
        if credential.expires_at <= now() + 60 {
            let token = json_response(http.post(&self.0.token_endpoint).form(&[
                ("client_id", self.0.oauth.client_id.as_str()),
                ("client_secret", self.0.oauth.client_secret.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", &credential.refresh_token),
            ]))
            .await?;
            credential.access_token = required(&token, "access_token")?;
            if let Some(refresh) = token["refresh_token"].as_str().filter(|s| !s.is_empty()) {
                credential.refresh_token = refresh.into();
            }
            credential.expires_at = now()
                + token["expires_in"]
                    .as_u64()
                    .ok_or("Google 响应缺少有效期")?;
            stored["access_token"] = json!(credential.access_token);
            stored["refresh_token"] = json!(credential.refresh_token);
            stored["expires_at"] = json!(credential.expires_at);
            save_private(
                &self.0.path,
                &serde_json::to_vec(&stored).map_err(|_| "无法编码 Google 授权")?,
            )
            .map_err(|_| "无法保存 Google 授权")?;
        }
        let endpoint = &self.0.api_endpoint;
        let plan = if self.0.imported {
            let catalog = json_response(
                http.post(format!("{endpoint}/v1internal:fetchAvailableModels"))
                    .bearer_auth(&credential.access_token)
                    .header("User-Agent", USER_AGENT)
                    .json(&json!({"project":credential.project})),
            )
            .await?;
            let effort = thinking_override.or_else(|| stored["thinking"].as_str());
            Some(model_plan(model, effort, &catalog)?)
        } else {
            None
        };
        Ok(GeminiConfig::new("").connect(Transport {
            http,
            token: credential.access_token,
            project: credential.project,
            model: plan
                .as_ref()
                .map(|p| p.model.clone())
                .unwrap_or_else(|| model.into()),
            endpoint: endpoint.into(),
            plan,
        }))
    }
}

#[derive(Clone)]
struct ModelPlan {
    model: String,
    model_enum: String,
    thinking: Value,
}
fn model_plan(model: &str, effort: Option<&str>, catalog: &Value) -> Result<ModelPlan, String> {
    let models = catalog["models"]
        .as_object()
        .ok_or("Antigravity returned no model catalog")?;
    let suffixes: &[&str] = match effort.unwrap_or("high") {
        "off" => &[""],
        "minimal" => &["-low", "-extra-low", ""],
        "low" => &["-low", "-extra-low", ""],
        "medium" => &["-medium", "", "-high"],
        "high" => &["-high", "-thinking", "-agent", ""],
        "xhigh" => &["-xhigh", "-max", "-high", "-thinking", ""],
        "max" => &["-max", "-high", "-thinking", ""],
        _ => return Err("Unsupported thinking level".into()),
    };
    let selected = suffixes
        .iter()
        .map(|s| format!("{model}{s}"))
        .find(|id| models.contains_key(id))
        .ok_or_else(|| {
            format!("Antigravity model {model} is unavailable in the current account catalog")
        })?;
    let info = &models[&selected];
    let model_enum = required(info, "model")?;
    let thinking = if effort == Some("off") {
        json!({"includeThoughts":false,"thinkingBudget":0})
    } else if let Some(budget) = info["thinkingBudget"].as_u64() {
        json!({"includeThoughts":true,"thinkingBudget":budget})
    } else if let Some(level) = ["low", "medium", "high", "xhigh", "max"]
        .iter()
        .find(|level| selected.ends_with(&format!("-{level}")))
    {
        json!({"includeThoughts":true,"thinkingBudget":0,"thinkingLevel":level.to_uppercase()})
    } else {
        json!({"includeThoughts":false,"thinkingBudget":0})
    };
    Ok(ModelPlan {
        model: selected,
        model_enum,
        thinking,
    })
}

fn authorization_url(client_id: &str, state: &str, verifier: &str) -> String {
    let mut url =
        reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").expect("static URL");
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    url.query_pairs_mut().extend_pairs([
        ("client_id", client_id), ("redirect_uri", REDIRECT), ("response_type", "code"),
        ("scope", "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs"),
        ("state", state), ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("access_type", "offline"), ("prompt", "consent"),
    ]);
    url.into()
}

fn callback_code(target: &str, state: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(&format!("http://localhost:51121{target}"))
        .map_err(|_| "无效授权回调")?;
    if url.path() != "/oauth-callback" {
        return Err("无效授权回调路径".into());
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    if pairs
        .iter()
        .filter(|(k, v)| k == "state" && v == state)
        .count()
        != 1
        || pairs.iter().filter(|(k, _)| k == "state").count() != 1
    {
        return Err("授权回调 state 不匹配".into());
    }
    if pairs.iter().any(|(k, _)| k == "error") {
        return Err("Google 授权被拒绝".into());
    }
    let codes: Vec<_> = pairs.iter().filter(|(k, _)| k == "code").collect();
    if codes.len() != 1 || codes[0].1.is_empty() {
        return Err("授权回调缺少 code".into());
    }
    Ok(codes[0].1.to_string())
}

async fn receive_code(listener: TcpListener, state: &str) -> Result<String, String> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|_| "无法接收 Google 回调")?;
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 1024];
            while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                let count = socket.read(&mut chunk).await?;
                if count == 0 || bytes.len() + count > 8192 {
                    return Err(std::io::Error::other("invalid callback"));
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
        .await;
        let Ok(Ok(request)) = request else {
            continue;
        };
        let target = request
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|line| line.strip_suffix(" HTTP/1.1"));
        let result = target
            .ok_or_else(|| "无效回调请求".to_string())
            .and_then(|target| callback_code(target, state));
        let reply = if result.is_ok() {
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nAuthorization received. Return to Ravomi."
        } else {
            "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nInvalid authorization callback."
        };
        socket
            .write_all(reply.as_bytes())
            .await
            .map_err(|_| "无法完成授权回调")?;
        if result.as_ref().is_err_and(|e| e == "Google 授权被拒绝") {
            return result;
        }
        if let Ok(code) = result {
            return Ok(code);
        }
    }
}

pub fn antigravity_status(state: &Antigravity) -> Status {
    state
        .0
        .status
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

pub async fn antigravity_login(state: &Antigravity) -> Result<(), String> {
    let mut task = state.0.login.lock().await;
    if state.0.status.lock().map_err(|_| "授权状态不可用")?.pending {
        return Err("登录已在进行中".into());
    }
    let listener = TcpListener::bind("127.0.0.1:51121")
        .await
        .map_err(|_| "Google 登录回调端口 51121 被占用，请关闭其他 Antigravity 登录后重试")?;
    let csrf = nonce();
    let verifier = nonce();
    {
        let mut status = state.0.status.lock().map_err(|_| "授权状态不可用")?;
        status.pending = true;
        status.error = None;
        status.verification_uri = Some(authorization_url(
            &state.0.oauth.client_id,
            &csrf,
            &verifier,
        ));
    }
    let account = state.0.clone();
    *task = Some(tokio::spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(900), async {
            let _guard = account.operation.lock().await;
            let code = receive_code(listener, &csrf).await?;
            let http = http()?;
            let token = json_response(http.post(TOKEN).form(&[("client_id", account.oauth.client_id.as_str()), ("client_secret", account.oauth.client_secret.as_str()), ("grant_type", "authorization_code"), ("redirect_uri", REDIRECT), ("code", &code), ("code_verifier", &verifier)])).await?;
            let access = required(&token, "access_token")?;
            let project = json_response(http.post(format!("{API}/v1internal:loadCodeAssist")).bearer_auth(&access).json(&json!({"metadata":{"ideType":"ANTIGRAVITY","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}))).await?;
            let project = project["cloudaicompanionProject"].as_str().or_else(|| project.pointer("/cloudaicompanionProject/id").and_then(Value::as_str)).filter(|p| !p.is_empty()).ok_or("该账号未返回 Antigravity 项目，请先在官方应用完成开通")?.to_string();
            let credential = Credential { access_token: access, refresh_token: required(&token, "refresh_token")?, expires_at: now() + token["expires_in"].as_u64().ok_or("Google 响应缺少有效期")?, project };
            save_private(&account.path, &serde_json::to_vec(&credential).map_err(|_| "无法编码 Google 授权")?).map_err(|_| "无法保存 Google 授权")?;
            Ok::<_, String>(())
        }).await;
        let mut status = account.status.lock().unwrap_or_else(|p| p.into_inner());
        status.pending = false;
        status.verification_uri = None;
        match result {
            Ok(Ok(())) => {
                status.connected = true;
                status.error = None;
            }
            Ok(Err(error)) => status.error = Some(error),
            Err(_) => status.error = Some("Google 登录超时".into()),
        }
    }));
    Ok(())
}

pub fn antigravity_open_login(state: &Antigravity) -> Result<(), String> {
    let status = state.0.status.lock().map_err(|_| "授权状态不可用")?;
    crate::accounts::open_url(
        status
            .verification_uri
            .as_deref()
            .ok_or("请先开始 Google 登录")?,
    )
}

pub async fn antigravity_logout(state: &Antigravity) -> Result<(), String> {
    let mut task = state.0.login.lock().await;
    if let Some(task) = task.take() {
        task.abort();
    }
    let _guard = state.0.operation.lock().await;
    match std::fs::remove_file(&state.0.path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("无法移除 Google 授权".into()),
    }
    *state.0.status.lock().map_err(|_| "授权状态不可用")? = Status::default();
    Ok(())
}

#[derive(Clone)]
struct Transport {
    http: reqwest::Client,
    token: String,
    project: String,
    model: String,
    endpoint: String,
    plan: Option<ModelPlan>,
}
fn transport_error(message: &str) -> http_client::Error {
    http_client::Error::instance(std::io::Error::other(message.to_owned()))
}

impl HttpClientExt for Transport {
    fn send<T, U>(
        &self,
        _req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        T: Into<Bytes> + Send,
        U: From<Bytes> + Send + 'static,
    {
        std::future::ready(Err(transport_error(
            "Antigravity only supports streaming inference",
        )))
    }
    fn send_multipart<U>(
        &self,
        _req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + Send + 'static
    where
        U: From<Bytes> + Send + 'static,
    {
        std::future::ready(Err(transport_error(
            "Antigravity file upload is unsupported",
        )))
    }
    async fn send_streaming<T>(&self, req: Request<T>) -> http_client::Result<StreamingResponse>
    where
        T: Into<Bytes> + Send,
    {
        let mut request: Value = serde_json::from_slice(&req.into_body().into())
            .map_err(http_client::Error::instance)?;
        if let Some(plan) = &self.plan {
            request["generationConfig"]["thinkingConfig"] = plan.thinking.clone();
            request["labels"] = json!({"model_enum":plan.model_enum,"used_claude":self.model.starts_with("claude").to_string(),"used_non_gemini_model":(!self.model.starts_with("gemini")).to_string()});
        }
        let body = json!({"project":self.project,"model":self.model,"request":request,"requestType":"agent","userAgent":"antigravity","requestId":format!("agent-{}",uuid::Uuid::new_v4())});
        let response = self
            .http
            .post(format!(
                "{}/v1internal:streamGenerateContent?alt=sse",
                self.endpoint
            ))
            .bearer_auth(&self.token)
            .header(
                "User-Agent",
                if self.plan.is_some() {
                    USER_AGENT
                } else {
                    "Mio/0.1.0"
                },
            )
            .json(&body)
            .send()
            .await
            .map_err(http_client::Error::instance)?;
        if !response.status().is_success() {
            return Err(http_client::Error::non_success_with_details(
                response.status(),
                response.headers().clone(),
                "Antigravity request rejected".into(),
            ));
        }
        let output = stream::try_unfold(
            (
                response.bytes_stream(),
                SseFramer::new(),
                VecDeque::<Bytes>::new(),
                0usize,
            ),
            |(mut input, mut framer, mut queue, mut buffered)| async move {
                loop {
                    if let Some(bytes) = queue.pop_front() {
                        return Ok(Some((bytes, (input, framer, queue, buffered))));
                    }
                    let Some(chunk) = input.next().await else {
                        return Ok(None);
                    };
                    let chunk = chunk.map_err(http_client::Error::instance)?;
                    buffered += chunk.len();
                    if buffered > 8 * 1024 * 1024 {
                        return Err(transport_error("Antigravity event exceeds limit"));
                    }
                    for event in framer.push(&chunk) {
                        buffered = 0;
                        if event.data.trim().is_empty() {
                            continue;
                        }
                        let mut envelope: Value = serde_json::from_str(&event.data)
                            .map_err(http_client::Error::instance)?;
                        if envelope.get("error").is_some() {
                            return Err(transport_error("Antigravity stream returned an error"));
                        }
                        let response = envelope
                            .get_mut("response")
                            .ok_or_else(|| transport_error("Missing Antigravity response"))?
                            .take();
                        queue.push_back(Bytes::from(format!("data: {}\n\n", response)));
                    }
                }
            },
        );
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Box::pin(output) as http_client::BoxedStream)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_catalog_resolves_selected_model_and_thinking() {
        let catalog = json!({"models":{"gemini-test-high":{"model":"TEST_HIGH"},"gemini-test-low":{"model":"TEST_LOW","thinkingBudget":1024}}});
        let high = model_plan("gemini-test", Some("high"), &catalog).unwrap();
        assert_eq!(high.model, "gemini-test-high");
        assert_eq!(high.thinking["thinkingLevel"], "HIGH");
        assert_eq!(
            model_plan("gemini-test", Some("low"), &catalog)
                .unwrap()
                .thinking["thinkingBudget"],
            1024
        );
        assert!(model_plan("missing", Some("high"), &catalog).is_err());
    }
    #[tokio::test]
    async fn imported_account_refreshes_once_and_is_shared_with_native_children() {
        use std::io::Write;
        struct NoShell;
        impl crate::Host for NoShell {
            fn shell(
                &self,
                _: &str,
                _: &std::path::Path,
            ) -> crate::HostFuture<Result<String, String>> {
                Box::pin(async { Err("disabled".into()) })
            }
            fn on_event(&self, _: crate::AgentEvent) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let profile = json!({"access_token":"expired-access","refresh_token":"test-refresh","expires_at":1,"project":"test-project",
            "client_id":"test-client","client_secret":"test-secret","model":"gemini-test","thinking":"high"});
        save_private(
            &dir.path().join(crate::profile::PROFILE_FILE),
            &serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let (mut refreshes, mut catalogs, mut completions) = (0, 0, 0);
            for _ in 0..5 {
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let (header, body) = crate::test_http::read_body(&mut socket);
                let (response, content_type) = if header.starts_with("POST /token ") {
                    refreshes += 1;
                    assert!(
                        String::from_utf8(body)
                            .unwrap()
                            .contains("refresh_token=test-refresh")
                    );
                    (json!({"access_token":"new-access","refresh_token":"rotated-refresh","expires_in":3600}).to_string(), "application/json")
                } else {
                    assert!(
                        header
                            .to_lowercase()
                            .contains("authorization: bearer new-access")
                    );
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    if header.contains("fetchAvailableModels") {
                        catalogs += 1;
                        (
                            json!({"models":{"gemini-test-high":{"model":"TEST_HIGH"}}})
                                .to_string(),
                            "application/json",
                        )
                    } else {
                        completions += 1;
                        assert_eq!(body["model"], "gemini-test-high");
                        assert_eq!(
                            body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                            "HIGH"
                        );
                        assert!(!body.to_string().contains("subagent"));
                        (
                            format!(
                                "data: {}\n\n",
                                json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ACCOUNT_SHARED"}]},"finishReason":"STOP"}]}})
                            ),
                            "text/event-stream",
                        )
                    }
                };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
            assert_eq!((refreshes, catalogs, completions), (1, 2, 2));
        });
        let mut account = Antigravity::from_profile(dir.path().to_owned()).unwrap();
        let inner = Arc::get_mut(&mut account.0).unwrap();
        inner.token_endpoint = format!("{endpoint}/token");
        inner.api_endpoint = endpoint;
        let config = crate::Config {
            provider: crate::Provider::Antigravity,
            model: "gemini-test".into(),
            ..crate::Config::default()
        };
        let manager = crate::subagent::SubagentManager::new_with_antigravity(
            dir.path(),
            config.clone(),
            crate::RunOptions::default(),
            account.clone(),
        )
        .unwrap();
        crate::run(
            config,
            Vec::new(),
            "parent",
            dir.path(),
            Arc::new(NoShell),
            crate::Services {
                antigravity: Some(&account),
                ..crate::Services::default()
            },
        )
        .await
        .unwrap();
        let child = manager
            .delegate(
                crate::subagent::Delegate {
                    session: "child".into(),
                    goal: "verify".into(),
                    prompt: "child".into(),
                    path: None,
                    read_only: true,
                },
                Arc::new(NoShell),
            )
            .unwrap();
        let done = manager
            .status(child.run_id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            done.status,
            crate::subagent::Status::Completed,
            "{:?}",
            done.error
        );
        assert_eq!(done.output, "ACCOUNT_SHARED");
        let saved = crate::profile::load(dir.path()).unwrap();
        assert_eq!(saved.refresh_token, "rotated-refresh");
        assert_eq!(saved.client_secret, "test-secret");
        assert_eq!(saved.thinking.as_deref(), Some("high"));
        server.join().unwrap();
    }
    #[test]
    fn oauth_binds_state_and_keeps_pkce_verifier_off_the_wire() {
        let url = authorization_url("unit-test-client", "test-state", "private-verifier");
        assert!(!url.contains("private-verifier"));
        assert!(url.contains("client_id=unit-test-client"));
        assert!(!url.contains("unit-test-secret"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(callback_code("/oauth-callback?state=wrong&code=x", "test-state").is_err());
        assert!(
            callback_code(
                "/oauth-callback?state=test-state&state=test-state&code=x",
                "test-state"
            )
            .is_err()
        );
        assert!(
            callback_code(
                "/oauth-callback?state=test-state&error=access_denied",
                "test-state"
            )
            .is_err()
        );
        assert_eq!(
            callback_code("/oauth-callback?state=test-state&code=valid", "test-state").unwrap(),
            "valid"
        );
    }
    #[test]
    fn rejects_empty_oauth_credentials() {
        let temp_dir = std::env::temp_dir();
        assert!(
            Antigravity::new(
                temp_dir.clone(),
                OAuthConfig {
                    client_id: "".into(),
                    client_secret: "unit-test-secret".into(),
                }
            )
            .is_err()
        );
        assert!(
            Antigravity::new(
                temp_dir.clone(),
                OAuthConfig {
                    client_id: "   ".into(),
                    client_secret: "unit-test-secret".into(),
                }
            )
            .is_err()
        );
        assert!(
            Antigravity::new(
                temp_dir.clone(),
                OAuthConfig {
                    client_id: "unit-test-client".into(),
                    client_secret: "".into(),
                }
            )
            .is_err()
        );
        assert!(
            Antigravity::new(
                temp_dir.clone(),
                OAuthConfig {
                    client_id: "unit-test-client".into(),
                    client_secret: "   ".into(),
                }
            )
            .is_err()
        );
        assert!(
            Antigravity::new(
                temp_dir,
                OAuthConfig {
                    client_id: "unit-test-client".into(),
                    client_secret: "unit-test-secret".into(),
                }
            )
            .is_ok()
        );
    }
    #[tokio::test]
    async fn transport_wraps_tools_and_preserves_split_unicode_sse() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let (header, body) = crate::test_http::read_json(&mut socket);
            assert!(header.starts_with("POST /v1internal:streamGenerateContent?alt=sse "));
            assert!(
                header
                    .to_lowercase()
                    .contains("authorization: bearer test-token")
            );
            assert_eq!(body["project"], "test-project");
            assert_eq!(body["model"], "test-model");
            assert_eq!(
                body.pointer("/request/tools/0/functionDeclarations/0/name")
                    .unwrap(),
                "workspace"
            );
            let response = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"你好\"}]},\"finishReason\":\"STOP\"}]}}\r\n\r\n";
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
            for byte in response.as_bytes() {
                socket.write_all(&[*byte]).unwrap();
            }
        });
        let transport = Transport {
            http: http().unwrap(),
            token: "test-token".into(),
            project: "test-project".into(),
            model: "test-model".into(),
            endpoint,
            plan: None,
        };
        let request = Request::builder()
            .body(Bytes::from(
                json!({"tools":[{"functionDeclarations":[{"name":"workspace"}]}]}).to_string(),
            ))
            .unwrap();
        let response = transport.send_streaming(request).await.unwrap();
        let mut output = response.into_body();
        let mut all = Vec::new();
        while let Some(bytes) = output.next().await {
            all.extend_from_slice(&bytes.unwrap());
        }
        let text = String::from_utf8(all).unwrap();
        assert!(text.contains("你好"));
        assert!(text.contains("STOP"));
        assert!(!text.contains("\"response\""));
        server.join().unwrap();
    }
}

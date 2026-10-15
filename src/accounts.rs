//! Account credentials never cross IPC. Rig's cache is staged in a private
//! directory, then atomically persisted: interrupted refreshes retain the old file.
use mio_core::providers::{
    chatgpt::{
        self,
        auth::{AuthSource, Authenticator, DeviceCodeHandler},
    },
    openai::{OpenAI, OpenAIConfig},
};
use serde::Serialize;
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connected: bool,
    pub pending: bool,
    pub verification_uri: Option<String>,
    pub user_code: Option<String>,
    pub error: Option<String>,
}

pub struct Account {
    path: PathBuf,
    status: Mutex<Status>,
    operation: tokio::sync::Mutex<()>,
    login: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Accounts(pub Arc<Account>);

impl Accounts {
    pub fn new(directory: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = directory.join("codex.json");
        let connected = path.try_exists()?;
        Ok(Self(Arc::new(Account {
            path,
            status: Mutex::new(Status {
                connected,
                ..Status::default()
            }),
            operation: tokio::sync::Mutex::new(()),
            login: tokio::sync::Mutex::new(None),
        })))
    }

    pub async fn codex(&self) -> Result<OpenAI, String> {
        self.0.authenticate(false).await
    }
}

impl Account {
    async fn authenticate(self: &Arc<Self>, interactive: bool) -> Result<OpenAI, String> {
        let _guard = self.operation.lock().await;
        let directory = tempfile::tempdir().map_err(|_| "无法创建登录缓存")?;
        let stage = directory.path().join("auth.json");
        if !interactive {
            let bytes = std::fs::read(&self.path).map_err(|_| "请先登录 Codex / ChatGPT 账号")?;
            std::fs::write(&stage, bytes).map_err(|_| "无法准备登录缓存")?;
        }
        let account = self.clone();
        let handler = DeviceCodeHandler::new(move |prompt| {
            let mut status = account.status.lock().unwrap_or_else(|p| p.into_inner());
            status.verification_uri = Some(prompt.verification_uri);
            status.user_code = Some(prompt.user_code);
        });
        let auth = Authenticator::new(AuthSource::OAuth, Some(stage.clone()), handler, interactive);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| "无法创建授权连接")?;
        let client = OpenAIConfig::with_key(&chatgpt::DIALECT, "")
            .connect(mio_reqwest::ReqwestClient::from(http))
            .authenticate(&auth)
            .await
            .map_err(
                |_| "Codex 授权失败或已失效，请重新登录；设备登录需在 ChatGPT 安全设置中启用",
            )?;
        let bytes = std::fs::read(stage).map_err(|_| "无法读取授权结果")?;
        save_private(&self.path, &bytes).map_err(|_| "无法保存账号授权")?;
        Ok(client)
    }
}

pub(crate) fn save_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| std::io::Error::other("missing parent"))?,
    )?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn account_status(state: &Accounts) -> Status {
    state
        .0
        .status
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

pub async fn account_login(state: &Accounts) -> Result<(), String> {
    let account = state.0.clone();
    let mut login = account.login.lock().await;
    let mut status = account.status.lock().map_err(|_| "登录状态不可用")?;
    if status.pending {
        return Err("登录已在进行中".into());
    }
    status.pending = true;
    status.error = None;
    status.user_code = None;
    status.verification_uri = None;
    drop(status);
    let task_account = account.clone();
    *login = Some(tokio::spawn(async move {
        let result =
            tokio::time::timeout(Duration::from_secs(900), task_account.authenticate(true)).await;
        let mut status = task_account
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        status.pending = false;
        status.user_code = None;
        status.verification_uri = None;
        match result {
            Ok(Ok(_)) => {
                status.connected = true;
                status.error = None;
            }
            Ok(Err(error)) => status.error = Some(error),
            Err(_) => status.error = Some("登录已超时，请重试".into()),
        }
    }));
    Ok(())
}

pub async fn account_logout(state: &Accounts) -> Result<(), String> {
    let mut login = state.0.login.lock().await;
    if let Some(login) = login.take() {
        login.abort();
    }
    let _guard = state.0.operation.lock().await;
    match std::fs::remove_file(&state.0.path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("无法移除账号授权".into()),
    }
    *state.0.status.lock().map_err(|_| "登录状态不可用")? = Status::default();
    Ok(())
}

pub fn account_open_login() -> Result<(), String> {
    // A fixed destination: the WebView cannot ask Rust to launch arbitrary URLs.
    let url = "https://auth.openai.com/codex/device";
    open_url(url)
}

pub(crate) fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    result
        .map(|mut child| {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        })
        .map_err(|_| "无法打开浏览器，请复制界面中的登录地址".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_private_and_replaced_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(directory.path().join("accounts")).unwrap();
        save_private(&accounts.0.path, b"first").unwrap();
        save_private(&accounts.0.path, b"refreshed").unwrap();
        assert_eq!(std::fs::read(&accounts.0.path).unwrap(), b"refreshed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&accounts.0.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let status = serde_json::to_string(&*accounts.0.status.lock().unwrap()).unwrap();
        assert!(!status.contains("refreshed"));
    }
    #[tokio::test]
    async fn saved_account_resolves_without_login_or_exposing_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let accounts = Accounts::new(directory.path().join("accounts")).unwrap();
        assert!(accounts.codex().await.is_err());
        let record = br#"{"access_token":"local-test-token","refresh_token":"local-refresh","account_id":"local-account","expires_at":4102444800}"#;
        save_private(&accounts.0.path, record).unwrap();
        assert!(accounts.codex().await.is_ok());
        let retained: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&accounts.0.path).unwrap()).unwrap();
        assert_eq!(retained["refresh_token"], "local-refresh");
        assert!(
            !serde_json::to_string(&*accounts.0.status.lock().unwrap())
                .unwrap()
                .contains("local-test-token")
        );
    }
}

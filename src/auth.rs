//! `rvbench auth login / status / logout` 与本地凭据文件（设计 §5.4）。
//! 文件按 host 分段：本地 manager 与生产互不串台；写入用临时文件 + rename 原子替换。
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// GitHub OAuth App 的 client_id 非敏感，可内置；真值待 OAuth App 注册后替换。
const DEFAULT_CLIENT_ID: &str = "Iv1.TODO_REGISTER_OAUTH_APP";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const DEVICE_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const SCOPE: &str = "read:user read:org";

#[derive(Subcommand)]
pub enum Action {
    /// 登录：GitHub device flow，或 --static <login> 直连本地 static provider。
    Login {
        /// 直接用配置好的 static 用户名登录（本地开发/测试）。
        #[arg(long = "static", value_name = "LOGIN")]
        static_login: Option<String>,
        /// 覆盖 GitHub OAuth App 的 client_id。
        #[arg(long)]
        client_id: Option<String>,
    },
    /// 当前主体、角色与 token 概要（只显示前 8 位）。
    Status,
    /// 撤销服务端会话并删除本地凭据段。
    Logout {
        /// 服务端不可达时仍然删除本地段。
        #[arg(long)]
        force: bool,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthEntry {
    pub token: String,
    pub expires_at: i64,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub sub: String,
    pub login: String,
    pub base_url: String,
    pub client: String,
    pub login_at: i64,
}
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Clone)]
pub struct Credentials {
    #[serde(default)]
    pub auths: std::collections::BTreeMap<String, AuthEntry>,
    /// worker 段本次不实现，仅保持结构兼容。
    #[serde(default)]
    pub workers: std::collections::BTreeMap<String, Value>,
}

pub fn credentials_path() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("rvbench").join("credentials");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("rvbench").join("credentials")
}
pub fn load_credentials() -> Credentials {
    std::fs::read(credentials_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
/// 原子写：临时文件 + rename；POSIX 上收紧为 0600（Windows 靠 ACL，文档声明）。
pub fn save_credentials(c: &Credentials) -> Result<()> {
    let path = credentials_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(c)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(&path)?; // rename 不能覆盖已存在的目标
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}
/// `--base` URL 的 authority（host[:port]）作为凭据段 key。
pub fn host_key(base: &str) -> Result<String> {
    let url = reqwest::Url::parse(base).context("invalid --base URL")?;
    let host = url.host_str().context("--base has no host")?;
    Ok(match url.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    })
}
/// token 优先级：环境变量 RVBENCH_TOKEN > 凭据文件段（设计 §5.4）。
pub fn resolve_token(base: &str) -> Option<(String, &'static str)> {
    if let Ok(t) = std::env::var("RVBENCH_TOKEN")
        && !t.trim().is_empty()
    {
        return Some((t, "env"));
    }
    let key = host_key(base).ok()?;
    load_credentials()
        .auths
        .get(&key)
        .map(|e| (e.token.clone(), "file"))
}
pub fn bearer_header(base: &str) -> Option<String> {
    resolve_token(base).map(|(t, _)| format!("Bearer {t}"))
}
fn prefix(token: &str) -> String {
    let head: String = token.chars().take(8).collect();
    format!("{head}…")
}
fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?)
}
fn error_message(value: &Value, status: reqwest::StatusCode) -> String {
    let nested = &value["error"];
    let msg = if nested.is_object() {
        nested["message"].as_str().unwrap_or("").to_string()
    } else {
        value["error"].as_str().unwrap_or("").to_string()
    };
    if msg.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {msg}")
    }
}
fn device_flow(client_id: &str) -> Result<String> {
    let http = http()?;
    let challenge: Value = http
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()?
        .error_for_status()?
        .json()?;
    let user_code = challenge["user_code"].as_str().context("device flow: no user_code")?;
    let device_code = challenge["device_code"].as_str().context("device flow: no device_code")?;
    let verification = challenge["verification_uri"]
        .as_str()
        .unwrap_or("https://github.com/login/device");
    let mut interval = challenge["interval"].as_u64().unwrap_or(5);
    println!("打开 {verification} 并输入代码：{user_code}");
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        let r: Value = http
            .post(DEVICE_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[("client_id", client_id), ("device_code", device_code), ("grant_type", "urn:ietf:params:oauth:grant-type:device_code")])
            .send()?
            .error_for_status()?
            .json()?;
        match r["error"].as_str() {
            None => {
                return r["access_token"]
                    .as_str()
                    .map(str::to_string)
                    .context("device flow: no access_token")
            }
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some(e) => bail!("device flow failed: {e}"),
        }
    }
}

pub fn run(base: Option<&str>, action: &Action) -> Result<()> {
    let base_url = base
        .unwrap_or("http://127.0.0.1:18800")
        .trim_end_matches('/')
        .to_string();
    let key = host_key(&base_url)?;
    match action {
        Action::Login {
            static_login,
            client_id,
        } => {
            let access_token = match static_login {
                Some(login) => login.clone(),
                None => {
                    let id = client_id
                        .clone()
                        .or_else(|| std::env::var("RVBENCH_CLIENT_ID").ok())
                        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
                    device_flow(&id)?
                }
            };
            let http = http()?;
            let response = http
                .post(format!("{base_url}/auth/github/exchange"))
                .json(&json!({"access_token": access_token}))
                .send()?;
            let status = response.status();
            let value: Value = response.json()?;
            ensure!(status.is_success(), "{}", error_message(&value, status));
            let expires_at = chrono::DateTime::parse_from_rfc3339(
                value["expires_at"].as_str().unwrap_or_default(),
            )
            .map(|d| d.timestamp())
            .unwrap_or(0);
            let entry = AuthEntry {
                token: value["token"].as_str().context("exchange: no token")?.to_string(),
                expires_at,
                refresh_token: None,
                sub: value["principal"]["sub"].as_str().unwrap_or("").to_string(),
                login: value["principal"]["login"].as_str().unwrap_or("").to_string(),
                base_url: base_url.clone(),
                client: format!("rvbench-cli/{}", env!("CARGO_PKG_VERSION")),
                login_at: now(),
            };
            let roles: Vec<&str> = value["principal"]["roles"]
                .as_array()
                .map(|a| a.iter().filter_map(|r| r.as_str()).collect())
                .unwrap_or_default();
            let mut credentials = load_credentials();
            credentials.auths.insert(key.clone(), entry.clone());
            save_credentials(&credentials)?;
            println!("host          {base_url}");
            println!("user          {} ({})", entry.login, entry.sub);
            println!("roles         {}", roles.join(", "));
            println!("user token    {}   expires {}", prefix(&entry.token), value["expires_at"].as_str().unwrap_or("?"));
            println!("token source  file");
            Ok(())
        }
        Action::Status => {
            let Some((token, source)) = resolve_token(&base_url) else {
                bail!("未登录：先运行 rvbench auth login（或设置 RVBENCH_TOKEN）");
            };
            let credentials = load_credentials();
            let entry = credentials.auths.get(&key);
            if let Some(e) = entry
                && e.expires_at > 0
                && e.expires_at < now()
            {
                println!("token 已过期（{}），请重新 rvbench auth login", value_date(e.expires_at));
            }
            let http = http()?;
            let response = http
                .get(format!("{base_url}/auth/me"))
                .header("Authorization", format!("Bearer {token}"))
                .send()?;
            let status = response.status();
            let value: Value = response.json()?;
            ensure!(status.is_success(), "{}", error_message(&value, status));
            println!("host          {base_url}");
            println!(
                "user          {} ({})",
                value["principal"]["login"].as_str().unwrap_or("?"),
                value["principal"]["sub"].as_str().unwrap_or("?")
            );
            let roles: Vec<&str> = value["principal"]["roles"]
                .as_array()
                .map(|a| a.iter().filter_map(|r| r.as_str()).collect())
                .unwrap_or_default();
            println!("roles         {}", roles.join(", "));
            println!("permissions   {}", value["permissions"].as_array().map(Vec::len).unwrap_or(0));
            println!("token         {}   source {source}", prefix(&token));
            println!(
                "expires       {}",
                value["session"]["expires_at"].as_str().unwrap_or("?")
            );
            if value["roles_stale"].as_bool() == Some(true) {
                println!("note          角色校验失败，暂按普通用户权限运行");
            }
            Ok(())
        }
        Action::Logout { force } => {
            let mut credentials = load_credentials();
            let Some(entry) = credentials.auths.get(&key).cloned() else {
                bail!("host {key} 没有本地凭据");
            };
            let outcome = http()?
                .post(format!("{base_url}/auth/logout"))
                .header("Authorization", format!("Bearer {}", entry.token))
                .send();
            match outcome {
                Ok(r) if r.status().is_success() || r.status().as_u16() == 401 => {
                    // 401 = 会话已失效，本地照删。
                }
                Ok(r) => {
                    let status = r.status();
                    let value: Value = r.json().unwrap_or(Value::Null);
                    ensure!(
                        *force,
                        "撤销失败（{}）；重试或 --force 仅删本地",
                        error_message(&value, status)
                    );
                }
                Err(e) => ensure!(
                    *force,
                    "manager 不可达（{e}）；重试或 --force 仅删本地"
                ),
            }
            credentials.auths.remove(&key);
            save_credentials(&credentials)?;
            println!("已登出 {base_url}");
            Ok(())
        }
    }
}
fn value_date(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
pub(crate) mod test_env {
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(_name: &str) -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn entry(login: &str, token: &str) -> AuthEntry {
        AuthEntry {
            token: token.into(),
            expires_at: now() + 1000,
            refresh_token: None,
            sub: format!("github:{login}"),
            login: login.into(),
            base_url: "http://127.0.0.1:18800".into(),
            client: "rvbench-cli/test".into(),
            login_at: now(),
        }
    }
    #[test]
    fn two_hosts_coexist_and_base_selects_the_right_one() {
        assert_eq!(host_key("http://127.0.0.1:18800").unwrap(), "127.0.0.1:18800");
        assert_eq!(host_key("https://rvbench.hk.c4y.dev").unwrap(), "rvbench.hk.c4y.dev");
        let mut c = Credentials::default();
        c.auths.insert("127.0.0.1:18800".into(), entry("alice", "rvb_local"));
        c.auths.insert("rvbench.hk.c4y.dev".into(), entry("c4y", "rvb_prod"));
        assert_eq!(c.auths["127.0.0.1:18800"].login, "alice");
        assert_eq!(c.auths["rvbench.hk.c4y.dev"].login, "c4y");
    }
    #[test]
    fn save_is_atomic_and_keeps_other_sections() {
        // 用 APPDATA 指向临时目录，避免污染真实凭据文件；env 是进程级的，串行化。
        let _guard = crate::auth::test_env::ENV_LOCK.lock().unwrap();
        let dir = temp_home("save");
        unsafe { std::env::set_var("APPDATA", dir.path()) };
        let mut c = Credentials::default();
        c.auths.insert("h".into(), entry("alice", "rvb_x"));
        c.workers
            .insert("h".into(), json!({"worker_id": "k3-1"}));
        save_credentials(&c).unwrap();
        let loaded = load_credentials();
        assert_eq!(loaded, c, "workers 段在覆写 auths 后必须原样保留");
        // 再写一次：rename 覆盖路径必须成功（Windows 先 remove）。
        let mut c2 = c.clone();
        c2.auths.remove("h");
        save_credentials(&c2).unwrap();
        assert!(load_credentials().auths.is_empty());
        unsafe { std::env::remove_var("APPDATA") };
    }
    #[test]
    fn token_prefix_never_leaks_the_full_token() {
        let e = entry("alice", "rvb_0123456789abcdefghij");
        let p = prefix(&e.token);
        assert!(p.starts_with("rvb_0123") && p.ends_with('…') && !p.contains("abcdefghij"));
    }
    #[test]
    fn expiry_check_uses_local_clock() {
        let mut e = entry("alice", "rvb_x");
        assert!(e.expires_at > now(), "fresh token must not read as expired");
        e.expires_at = now() - 10;
        assert!(e.expires_at < now());
    }
    #[test]
    fn unknown_sections_are_tolerated() {
        let v = json!({"auths": {}, "future_section": {"x": 1}});
        let c: Credentials = serde_json::from_value(v).unwrap();
        assert!(c.auths.is_empty() && c.workers.is_empty());
    }
}

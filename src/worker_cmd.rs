//! `rvbench worker create / login / status / logout`：面向 manager 的 worker
//! 记录与凭据管理（worker 计划 §0 CLI 行）。与 worker.rs 的直连模式动作同组。
use crate::auth;
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_MANAGER: &str = "http://127.0.0.1:18800";

#[derive(Subcommand)]
pub enum Action {
    /// 创建 worker 记录并签发一次性 token（需要已登录，`auth login`）。
    Create {
        /// worker_id：小写字母、数字、- 和 _，1–64 字符。
        #[arg(long)]
        id: String,
        /// 公开 worker（服务所有任务）；仅管理员。
        #[arg(long)]
        public: bool,
    },
    /// 认领模式：向 manager 申请短码，人在浏览器确认后取回 token（板上运行）。
    Login {
        #[arg(long)]
        worker_id: String,
        #[arg(long)]
        board_id: String,
        #[arg(long)]
        backend: String,
        /// 逗号分隔：func,correctness,performance
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
    },
    /// 显示 worker 记录（owner / 可见性 / 状态 / token 前缀）。
    Status {
        /// 缺省取凭据文件里本 host 的 worker_id。
        #[arg(long)]
        id: Option<String>,
    },
    /// 撤销 worker（DELETE /workers/{id}，需 owner 登录）并删本地段。
    Logout {
        /// 服务端不可达时仅删本地。
        #[arg(long)]
        force: bool,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct WorkerEntry {
    #[serde(default)]
    pub worker_id: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub login_at: i64,
}
fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}
fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?)
}
fn err_message(v: &Value, status: reqwest::StatusCode) -> String {
    let nested = &v["error"];
    let msg = if nested.is_object() {
        nested["message"].as_str().unwrap_or("").to_string()
    } else {
        v["error"].as_str().unwrap_or("").to_string()
    };
    if msg.is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {msg}") }
}
fn read_workers() -> std::collections::BTreeMap<String, WorkerEntry> {
    serde_json::from_str(
        &std::fs::read_to_string(auth::credentials_path()).unwrap_or_default(),
    )
    .ok()
    .and_then(|v: Value| serde_json::from_value(v["workers"].clone()).ok())
    .unwrap_or_default()
}
fn write_workers(map: &std::collections::BTreeMap<String, WorkerEntry>) -> Result<()> {
    let mut c = auth::load_credentials();
    let workers: Value = serde_json::to_value(map)?;
    c.workers = serde_json::from_value(workers).unwrap_or_default();
    auth::save_credentials(&c)
}
fn prefix(t: &str) -> String {
    format!("{}…", t.chars().take(9).collect::<String>())
}

pub fn run(base: Option<&str>, action: &Action) -> Result<i32> {
    let base = base.unwrap_or(DEFAULT_MANAGER).trim_end_matches('/');
    let key = auth::host_key(base)?;
    match action {
        Action::Create { id, public } => {
            let Some((token, _)) = auth::resolve_token(base) else {
                bail!("创建 worker 需要登录：先运行 rvbench --base {base} auth login");
            };
            let response = http()?
                .post(format!("{base}/workers"))
                .bearer_auth(token)
                .json(&json!({"worker_id": id, "visibility": if *public {"public"} else {"private"}}))
                .send()?;
            let status = response.status();
            let v: Value = response.json()?;
            ensure!(status.is_success(), "{}", err_message(&v, status));
            let created = &v["worker"];
            println!("worker_id   {}", created["worker_id"].as_str().unwrap_or(id));
            println!("owner       {} ({})", created["owner"]["login"].as_str().unwrap_or("?"), created["owner"]["sub"].as_str().unwrap_or("?"));
            println!("visibility  {}", created["visibility"].as_str().unwrap_or("?"));
            println!("token       {}（只显示这一次）", v["token"]["token"].as_str().unwrap_or("?"));
            println!("下一步      把 token 配到板子：RVBENCH_WORKER_TOKEN 或 ~/.config/rvbench/worker-token（0600）");
            let mut map = read_workers();
            map.insert(
                key.clone(),
                WorkerEntry {
                    worker_id: created["worker_id"].as_str().unwrap_or(id).to_string(),
                    token: String::new(),
                    expires_at: None,
                    visibility: created["visibility"].as_str().unwrap_or("").to_string(),
                    owner: created["owner"]["sub"].as_str().unwrap_or("").to_string(),
                    login_at: now(),
                },
            );
            write_workers(&map)?;
            Ok(0)
        }
        Action::Login { worker_id, board_id, backend, kinds } => {
            let client = http()?;
            let begin: Value = client
                .post(format!("{base}/cluster/workers/claim"))
                .json(&json!({"worker_id": worker_id, "board_id": board_id, "backend": backend, "kinds": kinds}))
                .send()?
                .error_for_status()
                .context("认领请求被拒绝")?
                .json()?;
            let code = begin["code"].as_str().context("缺少 code")?;
            let device = begin["device_code"].as_str().context("缺少 device_code")?;
            let url = begin["verification_url"].as_str().unwrap_or("/claim");
            let mut interval = begin["interval"].as_u64().unwrap_or(5).max(1);
            println!("打开 {url} 并输入短码：{code}");
            let token = loop {
                std::thread::sleep(Duration::from_secs(interval));
                let r = client
                    .get(format!("{base}/cluster/workers/claim/{device}"))
                    .send()?;
                match r.status().as_u16() {
                    202 => continue,
                    200 => break r.json::<Value>()?,
                    410 => bail!("认领被拒绝或已过期；重新运行 worker login"),
                    429 => { interval = (interval + 5).min(30); continue; }
                    s => bail!("轮询异常 HTTP {s}"),
                }
            };
            let token = token["token"]
                .as_str()
                .filter(|t| t.starts_with("rvbw_"))
                .context("approved 响应缺少 token")?
                .to_string();
            // 记入 credentials.workers，并写到 worker 服务读取的 worker-token 文件。
            let mut map = read_workers();
            map.insert(key.clone(), WorkerEntry {
                worker_id: worker_id.clone(),
                token: token.clone(),
                expires_at: None,
                visibility: "private".into(),
                owner: String::new(),
                login_at: now(),
            });
            write_workers(&map)?;
            store_worker_token(&token)?;
            println!("worker      {worker_id}（{board_id}）");
            println!("token       {}（已写入 credentials 与 worker-token 文件）", prefix(&token));
            println!("token file  {}", worker_token_path().display());
            Ok(0)
        }
        Action::Status { id } => {
            let map = read_workers();
            let wid = match id {
                Some(w) => w.clone(),
                None => map.get(&key).map(|e| e.worker_id.clone())
                    .context("没有 --id，且本 host 无 worker 记录（先 worker login）")?,
            };
            let mut req = http()?.get(format!("{base}/workers/{wid}"));
            if let Some((t, _)) = auth::resolve_token(base) {
                req = req.bearer_auth(t);
            }
            let response = req.send()?;
            let status = response.status();
            let v: Value = response.json()?;
            ensure!(status.is_success(), "{}", err_message(&v, status));
            println!("host        {base}");
            println!("worker_id   {}", v["worker_id"].as_str().unwrap_or(&wid));
            println!("owner       {} ({})", v["owner"]["login"].as_str().unwrap_or("?"), v["owner"]["sub"].as_str().unwrap_or("?"));
            println!("visibility  {}", v["visibility"].as_str().unwrap_or("?"));
            println!("status      {}（last_seen {}）", v["status"].as_str().unwrap_or("?"), v["last_seen"].as_str().unwrap_or("—"));
            if let Some(e) = map.get(&key)
                && !e.token.is_empty()
            {
                println!("local token {}   source file", prefix(&e.token));
            } else if std::env::var("RVBENCH_WORKER_TOKEN").is_ok() {
                println!("local token (env) RVBENCH_WORKER_TOKEN");
            }
            Ok(0)
        }
        Action::Logout { force } => {
            let map = read_workers();
            let Some(entry) = map.get(&key).cloned() else {
                bail!("host {key} 没有本地 worker 记录");
            };
            ensure!(!entry.worker_id.is_empty(), "本地记录缺 worker_id；手动 DELETE /workers/<id> 撤销");
            if let Some((user, _)) = auth::resolve_token(base) {
                let r = http()?
                    .delete(format!("{base}/workers/{}", entry.worker_id))
                    .bearer_auth(user)
                    .send()?;
                if !r.status().is_success() && r.status().as_u16() != 404 && !*force {
                    let status = r.status();
                    let v: Value = r.json().unwrap_or(Value::Null);
                    bail!("撤销失败（{}）；重试或 --force 仅删本地", err_message(&v, status));
                }
            } else if !*force {
                bail!("没有可用的用户登录（auth login）；或用 --force 仅删本地");
            }
            let mut map = map;
            map.remove(&key);
            write_workers(&map)?;
            let _ = std::fs::remove_file(worker_token_path());
            println!("已登出 worker {} @ {base}", entry.worker_id);
            Ok(0)
        }
    }
}

/// 与 worker 服务相同的 token 文件位置（worker login 双写：credentials + 此文件）。
pub fn worker_token_path() -> std::path::PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return std::path::PathBuf::from(appdata).join("rvbench").join("worker-token");
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".config").join("rvbench").join("worker-token")
}
fn store_worker_token(token: &str) -> Result<()> {
    let path = worker_token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_sections_roundtrip_and_never_touch_auths() {
        let _guard = crate::auth::test_env::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("APPDATA", dir.path()) };
        let mut c = auth::load_credentials();
        c.auths.insert(
            "h".into(),
            crate::auth::AuthEntry {
                token: "rvb_x".into(),
                expires_at: i64::MAX,
                refresh_token: None,
                sub: "github:1".into(),
                login: "a".into(),
                base_url: "http://h".into(),
                client: "t".into(),
                login_at: 1,
            },
        );
        auth::save_credentials(&c).unwrap();
        let mut w = std::collections::BTreeMap::new();
        w.insert("h".into(), WorkerEntry { worker_id: "k3-1".into(), token: "rvbw_x".into(), ..Default::default() });
        write_workers(&w).unwrap();
        let again = auth::load_credentials();
        assert_eq!(again.auths["h"].token, "rvb_x", "writing workers must not clobber auths");
        assert_eq!(read_workers()["h"].worker_id, "k3-1");
        unsafe { std::env::remove_var("APPDATA") };
    }
}

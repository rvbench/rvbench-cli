//! `rb-cli init`：拉取组件并生成 hello-world 级的本地环境。
//!
//! 三种来源：GitHub tarball（默认，匿名拉公有仓；有 token 时走 API 拉私有仓）、
//! `git clone`（走本机 git 凭据）、本地检出（离线，供开发与测试）。
//! 产出自包含目录：配置、数据目录、可直接提交的 `JobRequest`、Makefile 与清单。
//! 清单 `rvbench.init.json` 记录每个组件的来源、ref、commit 与内容摘要。
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

const MANIFEST: &str = "rvbench.init.json";
const HELLO_WORLD: &str = "suites/kernels/hello-world";
/// 不参与内容摘要、也不从本地检出复制过来的目录。
const SKIPPED: [&str; 6] = [
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    "data",
    ".rb-init",
];

struct Repo {
    component: &'static str,
    slug: &'static str,
    checkout: &'static str,
}
const MANAGER_REPO: Repo = Repo {
    component: "manager",
    slug: "rvbench/rvbench-official-manager",
    checkout: "rvbench-official-manager",
};
const WORKER_REPO: Repo = Repo {
    component: "worker",
    slug: "rvbench/rvbench-spacemit-worker",
    checkout: "rvbench-spacemit-worker",
};
const SUITES_REPO: Repo = Repo {
    component: "suite",
    slug: "rvbench/rvbench-official-suites",
    checkout: "rvbench-official-suites",
};

#[derive(Clone, Copy, ValueEnum)]
pub enum Source {
    /// GitHub tarball（私有仓需要 GH_TOKEN / GITHUB_TOKEN）
    Github,
    /// git clone --depth 1（走本机 git 凭据，适合私有仓）
    Git,
    /// 本地检出（离线）
    Local,
}
impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Git => "git",
            Self::Local => "local",
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Backend {
    K3,
    K1,
}
impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Self::K3 => "k3",
            Self::K1 => "k1",
        }
    }
}

#[derive(Args)]
pub struct Init {
    /// Target directory; must be empty unless --force is given
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Fetch the worker (rvbench-spacemit-worker)
    #[arg(long)]
    worker: bool,
    /// Fetch the manager (rvbench-official-manager)
    #[arg(long)]
    manager: bool,
    /// Fetch the hello-world suite (rvbench-official-suites)
    #[arg(long)]
    suite: bool,
    /// Where components come from
    #[arg(long, value_enum, default_value_t = Source::Github)]
    source: Source,
    /// Branch, tag or commit to fetch
    #[arg(long = "ref", default_value = "main")]
    reference: String,
    /// Checkout root for --source local (default: RVBENCH_REPO_ROOT)
    #[arg(long)]
    local_root: Option<PathBuf>,
    /// Hardware backend used by the generated worker config and job
    #[arg(long, value_enum, default_value_t = Backend::K3)]
    backend: Backend,
    /// Listen address written into the generated configs
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Manager port
    #[arg(long, default_value_t = 18800)]
    manager_port: u16,
    /// Worker port
    #[arg(long, default_value_t = 18801)]
    worker_port: u16,
    /// Proxy used for GitHub downloads (e.g. http://127.0.0.1:7890)
    #[arg(long)]
    proxy: Option<String>,
    /// Run cargo build --release --locked on the fetched components
    #[arg(long)]
    build: bool,
    /// Overwrite an existing, non-empty target directory
    #[arg(long)]
    force: bool,
    /// Print the manifest instead of the human summary
    #[arg(long)]
    json: bool,
}

struct Selection {
    manager: bool,
    worker: bool,
    suite: bool,
}
impl Selection {
    /// 不给组件开关时生成 hello-world 全链路。
    fn new(options: &Init) -> Self {
        let any = options.manager || options.worker || options.suite;
        Self {
            manager: options.manager || !any,
            worker: options.worker || !any,
            suite: options.suite || !any,
        }
    }
}

/// 暂存目录，离开作用域即清理；`--force` 重跑时不会留下半成品。
struct Staging(PathBuf);
impl Staging {
    fn new(root: &Path) -> Result<Self> {
        let path = root.join(".rb-init");
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fetched {
    path: PathBuf,
    commit: Option<String>,
}

/// 已拉到暂存区、尚未落盘的组件。
struct Staged {
    repo: &'static Repo,
    fetched: Fetched,
}

pub fn run(options: &Init) -> Result<()> {
    let root = std::path::absolute(&options.dir)
        .with_context(|| format!("无法解析目录 {}", options.dir.display()))?;
    let existed = root.exists();
    let result = generate(options, &root);
    // 拉取失败且什么都没写出来时，不要留下一个空目录。
    if result.is_err() && !existed {
        let empty = fs::read_dir(&root)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if empty {
            let _ = fs::remove_dir_all(&root);
        }
    }
    result
}

fn generate(options: &Init, root: &Path) -> Result<()> {
    let selection = Selection::new(options);
    ensure!(
        !(selection.manager && selection.worker) || options.manager_port != options.worker_port,
        "--manager-port 与 --worker-port 不能相同"
    );
    prepare(root, options.force)?;
    let client = matches!(options.source, Source::Github)
        .then(|| http_client(options))
        .transpose()?;
    let staging = Staging::new(root)?;
    // 先全部拉到暂存区：任何一步失败都不往目标目录写半成品。
    // 私有仓排在前面，凭据问题尽早暴露。
    let mut wanted: Vec<&Repo> = Vec::new();
    if selection.worker {
        wanted.push(&WORKER_REPO);
    }
    if selection.suite {
        wanted.push(&SUITES_REPO);
    }
    if selection.manager {
        wanted.push(&MANAGER_REPO);
    }
    let mut staged = Vec::new();
    for repo in wanted {
        let fetched = fetch(repo, options, &staging.0, client.as_ref())?;
        if repo.component == "suite" {
            validate_hello_world(&fetched.path)?;
        }
        staged.push(Staged { repo, fetched });
    }
    // 落盘顺序固定，清单里的组件顺序与命令行开关无关。
    let mut components = Vec::new();
    for name in ["manager", "worker", "suite"] {
        let Some(component) = staged.iter().find(|item| item.repo.component == name) else {
            continue;
        };
        let fetched = &component.fetched;
        match name {
            "manager" => {
                let dest = root.join("manager");
                replace(&fetched.path, &dest)?;
                fs::create_dir_all(root.join("data/manager"))?;
                write_json(&dest.join("config.json"), &manager_config(options))?;
                components.push(record(
                    "manager",
                    component.repo,
                    options,
                    &dest,
                    fetched.commit.clone(),
                )?);
            }
            "worker" => {
                let dest = root.join("worker");
                replace(&fetched.path, &dest)?;
                fs::create_dir_all(root.join("data/worker"))?;
                write_json(
                    &dest.join("config.json"),
                    &worker_config(options, selection.manager),
                )?;
                components.push(record(
                    "worker",
                    component.repo,
                    options,
                    &dest,
                    fetched.commit.clone(),
                )?);
            }
            _ => {
                let dest = root.join(HELLO_WORLD);
                replace(&fetched.path.join(HELLO_WORLD), &dest)?;
                write_schemas(&root.join("schemas"))?;
                let spec = crate::load_json(dest.join("spec.json"))?;
                let mut entry = record(
                    "suite",
                    component.repo,
                    options,
                    &dest,
                    fetched.commit.clone(),
                )?;
                entry["suite"] = json!("hello-world");
                entry["spec_sha256"] = json!(crate::spec_digest(&spec));
                components.push(entry);
                let job = hello_job(options.backend);
                rvbench_protocol::validate("JobRequest", &job)
                    .context("生成的 hello-world JobRequest 不符合合同")?;
                write_json(&root.join("jobs/hello-world.json"), &job)?;
            }
        }
    }
    ensure!(
        components.len() == staged.len(),
        "内部错误：有组件已拉取但未落盘"
    );

    let manifest = json!({
        "schema": "rvbench.init/1",
        "root": root.display().to_string(),
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "cli_version": env!("CARGO_PKG_VERSION"),
        "backend": options.backend.as_str(),
        "source": options.source.as_str(),
        "ref": options.reference,
        "endpoints": endpoints(options, &selection),
        "components": components,
    });
    write(&root.join("Makefile"), &makefile(&selection))?;
    write(&root.join("README.md"), &readme(options, &selection, root))?;
    write(&root.join(".gitignore"), "data/\n.rb-init/\n")?;
    write_json(&root.join(MANIFEST), &manifest)?;

    let mut built = Vec::new();
    if options.build {
        for component in ["manager", "worker"] {
            if (component == "manager" && !selection.manager)
                || (component == "worker" && !selection.worker)
            {
                continue;
            }
            build(component, &root.join(component))?;
            built.push(component);
        }
    }

    if options.json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
        return Ok(());
    }
    summarize(root, &manifest, options, &selection, &built);
    Ok(())
}

fn prepare(root: &Path, force: bool) -> Result<()> {
    if root.exists() {
        ensure!(root.is_dir(), "{} 不是目录", root.display());
        if let Some(entry) = fs::read_dir(root)?.next() {
            let entry = entry?;
            ensure!(
                force,
                "{} 非空（{}）；换一个空目录，或加 --force 覆盖",
                root.display(),
                entry.file_name().to_string_lossy()
            );
        }
    }
    fs::create_dir_all(root)?;
    Ok(())
}

fn http_client(options: &Init) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(30))
        .user_agent(user_agent());
    if let Some(proxy) = &options.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("解析 --proxy")?);
    }
    Ok(builder.build()?)
}

fn user_agent() -> String {
    format!("rb-cli/{}", env!("CARGO_PKG_VERSION"))
}

fn github_token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.is_empty()))
}

fn fetch(
    repo: &Repo,
    options: &Init,
    staging: &Path,
    client: Option<&reqwest::blocking::Client>,
) -> Result<Fetched> {
    let dest = staging.join(repo.component);
    let commit = match options.source {
        Source::Github => {
            let client = client.expect("github 来源已创建 HTTP 客户端");
            let commit = api_commit(options, repo.slug, client);
            let bytes = download(options, repo.slug, client)?;
            extract(&bytes, &dest, 1)?;
            commit
        }
        Source::Git => {
            clone(options, repo.slug, &dest)?;
            git_head(&dest)
        }
        Source::Local => {
            let source = local_source(options, repo)?;
            copy_tree(&source, &dest)?;
            git_head(&source)
        }
    };
    ensure!(
        fs::read_dir(&dest)?.next().is_some(),
        "{} 的 {} 拉取结果为空",
        repo.slug,
        options.reference
    );
    Ok(Fetched { path: dest, commit })
}

fn download(options: &Init, slug: &str, client: &reqwest::blocking::Client) -> Result<Vec<u8>> {
    let token = github_token();
    let url = match token {
        Some(_) => format!(
            "https://api.github.com/repos/{slug}/tarball/{}",
            options.reference
        ),
        None => format!(
            "https://codeload.github.com/{slug}/tar.gz/{}",
            options.reference
        ),
    };
    let mut request = client.get(&url);
    if let Some(token) = &token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .with_context(|| format!("无法连接 GitHub 拉取 {slug}"))?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "拉取 {slug}@{} 失败：HTTP {status}{}",
        options.reference,
        if status == StatusCode::NOT_FOUND {
            "（私有仓需要 GH_TOKEN / GITHUB_TOKEN，或改用 --source git 走本机 git 凭据）"
        } else {
            ""
        }
    );
    Ok(response.bytes()?.to_vec())
}

fn api_commit(options: &Init, slug: &str, client: &reqwest::blocking::Client) -> Option<String> {
    let url = format!(
        "https://api.github.com/repos/{slug}/commits/{}",
        options.reference
    );
    let mut request = client.get(url);
    if let Some(token) = github_token() {
        request = request.bearer_auth(token);
    }
    let response = request.send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: Value = response.json().ok()?;
    value["sha"].as_str().map(str::to_owned)
}

/// 解开 GitHub tarball；`strip` 去掉仓库名前缀，逐条校验路径不允许 `..` 或绝对路径。
fn extract(bytes: &[u8], dest: &Path, strip: usize) -> Result<()> {
    fs::create_dir_all(dest)?;
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some(relative) = strip_components(&path, strip)? else {
            continue;
        };
        let target = dest.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            entry.unpack(&target)?;
        }
        // 符号链接等类型跳过：组件仓不依赖它们。
    }
    Ok(())
}

fn strip_components(path: &Path, strip: usize) -> Result<Option<PathBuf>> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            _ => bail!("压缩包包含非法路径：{}", path.display()),
        }
    }
    if parts.len() <= strip {
        return Ok(None);
    }
    Ok(Some(parts[strip..].iter().collect()))
}

fn clone(options: &Init, slug: &str, dest: &Path) -> Result<()> {
    let url = format!("https://github.com/{slug}.git");
    let shallow = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--single-branch",
            "--branch",
            &options.reference,
            &url,
        ])
        .arg(dest)
        .output()
        .context("无法执行 git（请确认已安装）")?;
    if shallow.status.success() {
        return Ok(());
    }
    // ref 不是分支或标签（例如提交号）：先建仓再按 ref 取一次。
    let _ = fs::remove_dir_all(dest);
    let dest_text = dest.to_string_lossy().into_owned();
    git(None, &["init", "--quiet", &dest_text])?;
    git(Some(dest), &["remote", "add", "origin", &url])?;
    git(
        Some(dest),
        &["fetch", "--depth", "1", "origin", &options.reference],
    )?;
    git(
        Some(dest),
        &["checkout", "--detach", "--quiet", "FETCH_HEAD"],
    )
}

fn git(cwd: Option<&Path>, args: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .args(args)
        .output()
        .context("无法执行 git（请确认已安装）")?;
    ensure!(
        output.status.success(),
        "git {} 失败：{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn git_head(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_owned())
}

fn local_source(options: &Init, repo: &Repo) -> Result<PathBuf> {
    let root = options
        .local_root
        .clone()
        .or_else(|| env::var_os("RVBENCH_REPO_ROOT").map(PathBuf::from))
        .ok_or_else(|| anyhow::anyhow!("--source local 需要 --local-root 或 RVBENCH_REPO_ROOT"))?;
    let source = root.join(repo.checkout);
    ensure!(source.is_dir(), "本地检出不存在：{}", source.display());
    Ok(source)
}

fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if SKIPPED.contains(&name.as_str()) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn replace(staged: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        fs::remove_dir_all(dest).with_context(|| format!("无法清理旧目录 {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(staged, dest)
        .with_context(|| format!("无法移动 {} 到 {}", staged.display(), dest.display()))
}

fn record(
    name: &str,
    repo: &Repo,
    options: &Init,
    path: &Path,
    commit: Option<String>,
) -> Result<Value> {
    let (files, sha256) = tree_digest(path)?;
    Ok(json!({
        "component": name,
        "repo": repo.slug,
        "source": options.source.as_str(),
        "ref": options.reference,
        "commit": commit,
        "files": files,
        "sha256": sha256,
    }))
}

/// 内容摘要：按相对路径排序后对 `路径\0文件sha256` 取 SHA-256，跨机器稳定。
fn tree_digest(root: &Path) -> Result<(usize, String)> {
    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort();
    let mut text = String::new();
    for relative in &files {
        let bytes = fs::read(root.join(relative))?;
        text.push_str(relative);
        text.push('\0');
        text.push_str(&crate::json::sha256(&bytes));
        text.push('\n');
    }
    Ok((files.len(), crate::json::sha256(text.as_bytes())))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if SKIPPED.contains(&name.as_str()) {
                continue;
            }
            collect(root, &path, out)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root)?;
            out.push(
                relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
    Ok(())
}

fn manager_config(options: &Init) -> Value {
    json!({
        "listen": format!("{}:{}", options.bind, options.manager_port),
        // 相对路径：环境整体搬到板端后仍然可用，前提是从环境根目录启动。
        "data_dir": "data/manager",
        "victoriametrics": null,
    })
}

fn worker_config(options: &Init, with_manager: bool) -> Value {
    let host = reachable_host(&options.bind);
    json!({
        "host_metrics_url": null,
        "listen": format!("{}:{}", options.bind, options.worker_port),
        "url": format!("http://{host}:{}", options.worker_port),
        "worker_id": format!("local-{}", options.backend.as_str()),
        "board_id": "local-hello-world",
        "data_dir": "data/worker",
        "kinds": ["func", "correctness", "performance"],
        "delivery": "hybrid",
        "manager": with_manager.then(|| format!("http://{host}:{}", options.manager_port)),
        "backend": {
            "backend": options.backend.as_str(),
            "cc": "gcc",
            "python": "python3",
            "environment": {},
            "cpus": null,
            "ai_thread": false,
        },
    })
}

fn reachable_host(bind: &str) -> &str {
    match bind {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    }
}

fn endpoints(options: &Init, selection: &Selection) -> Value {
    let host = reachable_host(&options.bind);
    let mut value = json!({});
    if selection.manager {
        value["manager"] = json!(format!("http://{host}:{}", options.manager_port));
    }
    if selection.worker {
        value["worker"] = json!(format!("http://{host}:{}", options.worker_port));
    }
    value
}

/// 落盘前先确认拉到的 suite 真能通过本地合同校验，避免交付一个坏样例。
fn validate_hello_world(path: &Path) -> Result<()> {
    let spec_path = path.join(HELLO_WORLD).join("spec.json");
    ensure!(
        spec_path.is_file(),
        "{} 缺少 {HELLO_WORLD}/spec.json",
        SUITES_REPO.slug
    );
    let spec = crate::load_json(&spec_path)?;
    let errors = crate::validate_spec(&spec, spec_path.parent());
    ensure!(
        errors.is_empty(),
        "拉取的 hello-world spec 未通过校验：{}",
        errors.join("; ")
    );
    Ok(())
}

fn write_schemas(dir: &Path) -> Result<()> {
    for (file, kind) in [
        ("spec.schema.json", "spec"),
        ("spec.hidden.schema.json", "hidden"),
        ("eval_results.schema.json", "results"),
        ("baseline_results.schema.json", "baseline-results"),
    ] {
        let text = crate::schema_text(kind).expect("内置 schema");
        write(&dir.join(file), text)?;
    }
    Ok(())
}

/// hello-world 示例作业：`source` 就是程序本体，worker 会把它写成 `candidate.py`（triton）
/// 或直接编译（native-c）。
fn hello_job(backend: Backend) -> Value {
    let (implementation, source) = match backend {
        Backend::K3 => ("triton", "def candidate(a, b):\n    return a + b\n"),
        Backend::K1 => (
            "native-c",
            "void candidate(const float*a,const float*b,float*out,int n){for(int i=0;i<n;i++)out[i]=a[i]+b[i];}\n",
        ),
    };
    json!({
        "implementation": implementation,
        "backend": backend.as_str(),
        "kind": "correctness",
        "op": "add",
        "shape": [1024, 1024],
        "dtype": "float32",
        "seed": 20261017,
        "warmup": 2,
        "repetitions": 5,
        "timeout_secs": 180,
        "source": source,
        "attributes": {"suite": "hello-world", "case": "hello-world.add.C01"},
    })
}

fn makefile(selection: &Selection) -> String {
    let mut phony = Vec::new();
    let mut body = String::new();
    if selection.manager || selection.worker {
        phony.push("build");
        body.push_str("build:\n");
        if selection.manager {
            body.push_str("\tcargo build --release --locked --manifest-path manager/Cargo.toml\n");
        }
        if selection.worker {
            body.push_str("\tcargo build --release --locked --manifest-path worker/Cargo.toml\n");
        }
        body.push('\n');
    }
    if selection.manager {
        phony.push("manager");
        body.push_str(
            "manager:\n\tmanager/target/release/rvbench-manager$(EXE) --config manager/config.json\n\n",
        );
    }
    if selection.worker {
        phony.push("worker");
        body.push_str(
            "worker:\n\tworker/target/release/rvbench-worker$(EXE) --config worker/config.json\n\n",
        );
    }
    if selection.suite {
        phony.push("validate");
        body.push_str(&format!(
            "validate:\n\t$(RVBENCH) validate spec {HELLO_WORLD}/spec.json\n\n"
        ));
        phony.push("submit");
        body.push_str("submit:\n\t$(RVBENCH) control submit jobs/hello-world.json --wait\n\n");
    }
    format!(
        "# 由 rb-cli init 生成；组件与来源见 {MANIFEST}。\n\
         # 配置里的 data_dir 是相对路径，请在本目录执行这些目标。\n\n\
         EXE := $(if $(filter Windows_NT,$(OS)),.exe,)\n\
         RVBENCH ?= rvbench\n\n\
         .PHONY: {}\n\n{body}",
        phony.join(" ")
    )
}

fn readme(options: &Init, selection: &Selection, root: &Path) -> String {
    let mut layout = String::new();
    if selection.manager {
        layout.push_str("| `manager/` | manager 服务源码与 `config.json` |\n");
    }
    if selection.worker {
        layout.push_str("| `worker/` | worker 服务源码与 `config.json` |\n");
    }
    if selection.suite {
        layout.push_str(
            "| `suites/kernels/hello-world/` | hello-world suite（spec 字节与官方仓一致） |\n",
        );
        layout.push_str("| `schemas/` | 合同 schema 副本，供 spec 的 `$schema` 解析 |\n");
        layout.push_str(
            "| `jobs/hello-world.json` | 可直接提交的 `JobRequest`，`source` 即候选程序 |\n",
        );
    }
    layout.push_str("| `data/` | 服务数据目录（相对 `config.json` 所在根目录） |\n");
    layout.push_str(&format!(
        "| `{MANIFEST}` | 组件来源、ref、commit 与内容摘要 |\n"
    ));
    layout.push_str("| `Makefile` | 构建 / 启动 / 校验 / 提交 |\n");

    let mut steps = String::new();
    let mut step = 1;
    if selection.manager || selection.worker {
        steps.push_str(&format!(
            "{step}. 构建：`make build`（等价于对 manager/worker 各跑一次 `cargo build --release --locked`）\n"
        ));
        step += 1;
    }
    if selection.manager {
        steps.push_str(&format!(
            "{step}. 终端 A：`make manager`，监听 `{}:{}`\n",
            options.bind, options.manager_port
        ));
        step += 1;
    }
    if selection.worker {
        let manager = if selection.manager {
            format!(
                "，注册到 `http://{}:{}`",
                reachable_host(&options.bind),
                options.manager_port
            )
        } else {
            "（`manager: null`，独立模式）".into()
        };
        steps.push_str(&format!(
            "{step}. 终端 B：`make worker`，监听 `{}:{worker_port}`{manager}\n",
            options.bind,
            worker_port = options.worker_port
        ));
        step += 1;
    }
    if selection.suite {
        steps.push_str(&format!("{step}. 校验用例：`make validate`\n"));
        step += 1;
        steps.push_str(&format!("{step}. 提交作业：`make submit`（`rvbench control submit jobs/hello-world.json --wait`）\n"));
    }

    format!(
        "# rvbench hello-world 环境\n\n\
         由 `rb-cli init` 生成，路径 `{}`。目标后端 `{}`，组件来源 `{}`。\n\n\
         ## 目录\n\n| 路径 | 内容 |\n| --- | --- |\n{layout}\n\
         ## 跑通\n\n{steps}\n\
         ## 说明\n\n\
         - `config.json` 里的 `data_dir` 是**相对路径**，服务必须从本目录启动；整个目录可以原样搬到板端。\n\
         - 端口冲突时先用 `rb-cli init` 的 `--manager-port` / `--worker-port` 重新生成，或直接改两个 `config.json`。\n\
         - Makefile 默认调用 `rvbench`；只装了别名时用 `make validate RVBENCH=rb-cli`（两个 bin 同一入口）。\n\
         - `jobs/hello-world.json` 的 `source` 是请求字段：triton 后端会写成 `candidate.py` 并调用 `candidate(*inputs)`；native-c 后端直接编译。换程序只改这里。\n\
         - 重新生成：`rb-cli init . --force` 会重拉组件并覆盖 `config.json`，`data/` 保留；`--source git` 拉下来的目录带 `.git`，也可以直接 `git pull`。\n\
         - 私有仓（worker、suite）匿名 tarball 不可用：设 `GH_TOKEN`，或改用 `--source git` / `--source local`。\n",
        root.display(),
        options.backend.as_str(),
        options.source.as_str(),
    )
}

fn build(component: &str, dir: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .args(["build", "--release", "--locked"])
        .current_dir(dir)
        .status()
        .with_context(|| format!("无法执行 cargo（构建 {component}）"))?;
    ensure!(status.success(), "{component} 构建失败");
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text).with_context(|| format!("无法写入 {}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write(path, &text)
}

fn summarize(root: &Path, manifest: &Value, options: &Init, selection: &Selection, built: &[&str]) {
    println!("已生成 hello-world 环境：{}", root.display());
    println!();
    for component in manifest["components"].as_array().unwrap_or(&Vec::new()) {
        let name = component["component"].as_str().unwrap_or("");
        let source = component["source"].as_str().unwrap_or("");
        let reference = component["ref"].as_str().unwrap_or("");
        let commit = component["commit"]
            .as_str()
            .map(|sha| format!(" @{}", &sha[..sha.len().min(7)]))
            .unwrap_or_default();
        let files = component["files"].as_u64().unwrap_or(0);
        println!(
            "  {name:<8} {:<40} {source}:{reference}{commit}  {files} 个文件",
            component["repo"].as_str().unwrap_or("")
        );
    }
    println!();
    println!("下一步（在 {} 执行）：", root.display());
    if selection.manager || selection.worker {
        if built.is_empty() {
            println!("  make build     # 构建 manager / worker");
        } else {
            println!("  已构建：{}", built.join(", "));
        }
        if selection.manager {
            println!("  make manager   # 终端 A，监听 :{}", options.manager_port);
        }
        if selection.worker {
            println!("  make worker    # 终端 B，监听 :{}", options.worker_port);
        }
    }
    if selection.suite {
        println!("  make validate  # 校验 hello-world spec");
        println!("  make submit    # 提交 jobs/hello-world.json");
    }
}

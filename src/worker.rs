//! Direct-to-worker submission: post a job JSON to a worker `POST /jobs`, no manager.
//!
//! [`crate::control`] speaks the manager queue and cluster endpoints; this module only
//! touches the worker surface: `/jobs`, `/capabilities`, `/metrics`.
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use rvbench_protocol::{JobRecord, WorkerSubmission, terminal, validate};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const DEFAULT_WORKER: &str = "http://127.0.0.1:18801";
const WAIT_BUDGET: Duration = Duration::from_secs(3600);
/// Exit code when `submit` reaches a terminal state other than `done`; 0 stays success.
pub const EXIT_NOT_DONE: i32 = 3;

#[derive(Subcommand)]
pub enum Action {
    /// Submit a job file to this worker (`POST /jobs`).
    Submit {
        /// Bare `JobRequest`, or a `{"request": ...}` envelope. Both are accepted.
        file: PathBuf,
        /// Block until the job reaches a terminal state.
        #[arg(long)]
        wait: bool,
        /// Print only `<job_id> <state>`, for scripts.
        #[arg(long)]
        quiet: bool,
    },
    /// Read one job.
    Get {
        job_id: String,
        #[arg(long)]
        wait: bool,
    },
    /// Cancel a queued or running job.
    Cancel { job_id: String },
    /// Print the worker's `WorkerRegistration`.
    Capabilities,
    /// Print the worker's Prometheus metrics as plain text.
    Metrics,
}

pub fn run(base: Option<&str>, action: &Action) -> Result<i32> {
    let base = base.unwrap_or(DEFAULT_WORKER).trim_end_matches('/');
    let client = client_for(base)?;
    match action {
        Action::Submit { file, wait, quiet } => {
            let submission = read_submission(file)?;
            let response = client
                .post(format!("{base}/jobs"))
                .json(&serde_json::to_value(submission)?)
                .send()?;
            let mut value = body(response)?;
            if *wait {
                value = poll(&client, base, value)?;
            }
            let record: JobRecord = serde_json::from_value(value.clone())?;
            let state = state_text(&record.state)?;
            if *quiet {
                println!("{} {}", record.job_id, state);
            } else {
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            let finished = terminal(&record.state);
            Ok(if finished && state != "done" {
                EXIT_NOT_DONE
            } else {
                0
            })
        }
        Action::Get { job_id, wait } => {
            let response = client.get(format!("{base}/jobs/{job_id}")).send()?;
            let mut value = body(response)?;
            if *wait {
                value = poll(&client, base, value)?;
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Action::Cancel { job_id } => {
            let value = body(client.delete(format!("{base}/jobs/{job_id}")).send()?)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Action::Capabilities => {
            let value = body(client.get(format!("{base}/capabilities")).send()?)?;
            validate("WorkerRegistration", &value)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(0)
        }
        Action::Metrics => {
            let response = client.get(format!("{base}/metrics")).send()?;
            let status = response.status();
            let text = response.text()?;
            ensure!(status.is_success(), "HTTP {status}: {text}");
            print!("{text}");
            Ok(0)
        }
    }
}

/// Read a job file. A top-level `request` key means `WorkerSubmission`, else a bare `JobRequest`.
fn read_submission(path: &Path) -> Result<WorkerSubmission> {
    let value = crate::load_json(path)?;
    if value.get("request").is_some() {
        validate("WorkerSubmission", &value)
            .with_context(|| format!("{} is not a valid WorkerSubmission", path.display()))?;
        serde_json::from_value(value)
            .with_context(|| format!("{} failed to decode", path.display()))
    } else {
        validate("JobRequest", &value)
            .with_context(|| format!("{} is not a valid JobRequest", path.display()))?;
        Ok(WorkerSubmission {
            request: serde_json::from_value(value)
                .with_context(|| format!("{} failed to decode", path.display()))?,
            assignment: None,
        })
    }
}

/// A loopback worker must not go through `HTTP_PROXY`; remote workers keep the system proxy.
fn client_for(base: &str) -> Result<reqwest::blocking::Client> {
    let loopback = reqwest::Url::parse(base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"));
    let builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(35));
    Ok(if loopback {
        builder.no_proxy()
    } else {
        builder
    }
    .build()?)
}

/// Read a JSON body. Failures prefer the contract `error` field over the raw value.
fn body(response: reqwest::blocking::Response) -> Result<Value> {
    let status = response.status();
    let bytes = response.bytes()?;
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => anyhow::bail!("HTTP {status} with a non-JSON body: {}", snippet(&bytes)),
    };
    if !status.is_success() {
        let detail = value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        anyhow::bail!("HTTP {status}: {detail}");
    }
    Ok(value)
}

fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    trimmed.chars().take(200).collect()
}

/// The wire spelling of `JobState` (`done` / `failed` / ...).
fn state_text(state: &rvbench_protocol::JobState) -> Result<String> {
    Ok(serde_json::to_value(state)?
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

/// Poll to a terminal state. `?wait=25` long-polls on the worker side; this only bounds time.
fn poll(client: &reqwest::blocking::Client, base: &str, mut value: Value) -> Result<Value> {
    validate("JobRecord", &value)?;
    let until = Instant::now() + WAIT_BUDGET;
    loop {
        let record: JobRecord = serde_json::from_value(value.clone())?;
        if terminal(&record.state) {
            return Ok(value);
        }
        ensure!(
            Instant::now() < until,
            "wait exceeded one hour; the job is still queryable: {}",
            record.job_id
        );
        value = body(
            client
                .get(format!("{base}/jobs/{}?wait=25", record.job_id))
                .send()?,
        )?;
        validate("JobRecord", &value)?;
    }
}

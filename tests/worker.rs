//! `rb-cli worker` 直连提交的端到端回归：用本地 TCP stub 顶替 worker，不发真实请求。
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    thread,
};

const JOB_ID: &str = "6f1e4d2a-1c3b-4a59-8d7e-2b0c9f3a5e71";

struct Stub {
    url: String,
    bodies: Arc<Mutex<Vec<String>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start(responses: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        let handle = thread::spawn(move || {
            for (status, payload) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                if let Some((head, body)) = request.split_once("\r\n\r\n")
                    && head.starts_with("POST ")
                {
                    seen.lock().unwrap().push(body.to_owned());
                }
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            url,
            bodies,
            handle: Some(handle),
        }
    }

    fn join(mut self) -> Vec<String> {
        self.handle.take().unwrap().join().unwrap();
        self.bodies.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&data).to_string()
}

fn record(state: &str) -> String {
    format!(
        r#"{{"job_id":"{JOB_ID}","state":"{state}",
"request":{{"backend":"k1","kind":"correctness","op":"add","shape":[8,8],
"dtype":"float32","seed":1,"warmup":1,"repetitions":1,"timeout_secs":60,
"source":"void candidate(const float*a,const float*b,float*out,int n){{}}"}},
"worker_id":"w1","board_id":"b1","instance_id":"","submitted_at":"2026-10-20T05:00:00Z",
"logs":""}}"#
    )
}

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rvbench"))
}

fn write_job(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

const ENVELOPE: &str = r#"{"request":{"implementation":"native-c","backend":"k1","kind":"correctness",
"op":"add","shape":[8,8],"dtype":"float32","seed":1,"warmup":1,"repetitions":1,"timeout_secs":60,
"source":"void candidate(const float*a,const float*b,float*out,int n){}"}}"#;

const BARE: &str = r#"{"backend":"k1","kind":"correctness","op":"add","shape":[8,8],"dtype":"float32",
"seed":1,"warmup":1,"repetitions":1,"timeout_secs":60,
"source":"void candidate(const float*a,const float*b,float*out,int n){}"}"#;

#[test]
fn envelope_submits_without_assignment_and_waits_for_terminal_state() {
    let temp = tempfile::tempdir().unwrap();
    let job = write_job(temp.path(), "job.json", ENVELOPE);
    let stub = Stub::start(vec![(202, record("running")), (200, record("done"))]);
    let output = cli()
        .args(["--base", stub.url.as_str(), "worker", "submit"])
        .arg(&job)
        .args(["--wait", "--quiet"])
        .output()
        .unwrap();
    let seen = stub.join();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("{JOB_ID} done")
    );
    let posted: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
    assert!(
        posted.get("assignment").is_none(),
        "assignment must be omitted"
    );
    assert_eq!(posted["request"]["implementation"], "native-c");
}

#[test]
fn bare_job_request_is_accepted_and_failure_sets_exit_code() {
    let temp = tempfile::tempdir().unwrap();
    let job = write_job(temp.path(), "job.json", BARE);
    let stub = Stub::start(vec![(202, record("running")), (200, record("failed"))]);
    let output = cli()
        .args(["--base", stub.url.as_str(), "worker", "submit"])
        .arg(&job)
        .args(["--wait", "--quiet"])
        .output()
        .unwrap();
    let seen = stub.join();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("{JOB_ID} failed")
    );
    let posted: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
    assert!(posted.get("request").is_some(), "bare job must be wrapped");
}

#[test]
fn rejection_reports_the_contract_error() {
    let temp = tempfile::tempdir().unwrap();
    let job = write_job(temp.path(), "job.json", ENVELOPE);
    let rejection = r#"{"error":"implementation 'triton' is unavailable: no torch"}"#;
    let stub = Stub::start(vec![(409, rejection.to_owned())]);
    let output = cli()
        .args(["--base", stub.url.as_str(), "worker", "submit"])
        .arg(&job)
        .output()
        .unwrap();
    stub.join();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no torch"), "{stderr}");
}

#[test]
fn invalid_job_file_fails_before_any_request() {
    let temp = tempfile::tempdir().unwrap();
    let job = write_job(temp.path(), "job.json", r#"{"backend":"k1","kind":"nope"}"#);
    let stub = Stub::start(vec![]);
    let output = cli()
        .args(["--base", stub.url.as_str(), "worker", "submit"])
        .arg(&job)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("JobRequest"), "{stderr}");
}

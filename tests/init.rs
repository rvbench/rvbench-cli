//! `rb-cli init` 的离线回归：用本地检出作为来源，不访问网络。
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

const CLI: &str = env!("CARGO_BIN_EXE_rb-cli");

/// 造一套最小检出：两个 Rust 组件 + 只含 hello-world 的 suites 仓。
fn fixtures(root: &Path) {
    for (checkout, name) in [
        ("rvbench-official-manager", "rvbench-manager"),
        ("rvbench-spacemit-worker", "rvbench-worker"),
    ] {
        let path = root.join(checkout);
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(
            path.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\n"),
        )
        .unwrap();
        fs::write(path.join("README.md"), format!("# {name}\n")).unwrap();
        fs::write(path.join("src/main.rs"), "fn main() {}\n").unwrap();
    }
    let suite = root.join("rvbench-official-suites/suites/kernels/hello-world");
    fs::create_dir_all(&suite).unwrap();
    // 与官方仓同一布局：spec 的 $schema 相对路径指向仓根的 schemas/。
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello-world/spec.json");
    let mut spec: Value = rvbench_cli::load_json(&example).unwrap();
    spec["$schema"] = json!("../../../schemas/spec.schema.json");
    fs::write(
        suite.join("spec.json"),
        serde_json::to_vec_pretty(&spec).unwrap(),
    )
    .unwrap();
}

fn init(dir: &Path, root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(CLI)
        .current_dir(dir)
        .arg("init")
        .args(args)
        .arg("--source")
        .arg("local")
        .arg("--local-root")
        .arg(root)
        .output()
        .unwrap()
}

fn json(path: &Path) -> Value {
    rvbench_cli::load_json(path).unwrap()
}

#[test]
fn local_source_scaffolds_the_whole_hello_world_chain() {
    let temp = tempfile::tempdir().unwrap();
    let repos = temp.path().join("repos");
    fixtures(&repos);
    let env = temp.path().join("env");
    let output = init(temp.path(), &repos, &["env"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let manager: Value = json(&env.join("manager/config.json"));
    assert_eq!(manager["listen"], "127.0.0.1:18800");
    assert_eq!(manager["data_dir"], "data/manager");

    let worker: Value = json(&env.join("worker/config.json"));
    assert_eq!(worker["listen"], "127.0.0.1:18801");
    assert_eq!(worker["url"], "http://127.0.0.1:18801");
    assert_eq!(worker["manager"], "http://127.0.0.1:18800");
    assert_eq!(worker["backend"]["backend"], "k3");
    assert_eq!(worker["data_dir"], "data/worker");
    assert!(env.join("data/manager").is_dir() && env.join("data/worker").is_dir());

    let spec_path = env.join("suites/kernels/hello-world/spec.json");
    let spec = json(&spec_path);
    assert!(rvbench_cli::validate_spec(&spec, spec_path.parent()).is_empty());
    assert_eq!(
        fs::read_to_string(&spec_path).unwrap(),
        fs::read_to_string(
            repos.join("rvbench-official-suites/suites/kernels/hello-world/spec.json")
        )
        .unwrap()
    );
    for file in [
        "spec.schema.json",
        "spec.hidden.schema.json",
        "eval_results.schema.json",
        "baseline_results.schema.json",
    ] {
        assert!(env.join("schemas").join(file).is_file(), "{file}");
    }

    let job = json(&env.join("jobs/hello-world.json"));
    rvbench_protocol::validate("JobRequest", &job).unwrap();
    assert_eq!(job["op"], "add");
    assert_eq!(job["backend"], "k3");
    assert_eq!(job["implementation"], "triton");
    assert!(job["source"].as_str().unwrap().contains("def candidate"));

    let manifest = json(&env.join("rvbench.init.json"));
    assert_eq!(manifest["schema"], "rvbench.init/1");
    assert_eq!(manifest["components"].as_array().unwrap().len(), 3);
    assert_eq!(manifest["source"], "local");
    for component in manifest["components"].as_array().unwrap() {
        // 夹具不是 git 仓，commit 允许为 null；真实的本地检出会带上 HEAD。
        assert!(component["commit"].is_null() || component["commit"].as_str().unwrap().len() == 40);
        assert!(component["files"].as_u64().unwrap() > 0);
    }
    assert_eq!(
        manifest["components"][2]["spec_sha256"],
        rvbench_cli::spec_digest(&spec)
    );

    let makefile = fs::read_to_string(env.join("Makefile")).unwrap();
    assert!(makefile.contains(".PHONY: build manager worker validate submit"));
    assert!(
        makefile.contains("\tcargo build --release --locked --manifest-path worker/Cargo.toml")
    );
    let readme = fs::read_to_string(env.join("README.md")).unwrap();
    assert!(readme.contains("hello-world") && readme.contains("make submit"));
    assert!(!env.join(".rb-init").exists());
}

#[test]
fn worker_only_stays_standalone() {
    let temp = tempfile::tempdir().unwrap();
    let repos = temp.path().join("repos");
    fixtures(&repos);
    let env = temp.path().join("env");
    let output = init(temp.path(), &repos, &["env", "--worker", "--backend", "k1"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let worker: Value = json(&env.join("worker/config.json"));
    assert!(worker["manager"].is_null());
    assert_eq!(worker["backend"]["backend"], "k1");
    assert_eq!(worker["worker_id"], "local-k1");
    assert!(!env.join("manager").exists());
    assert!(!env.join("jobs").exists());
    assert!(!env.join("schemas").exists());
    let manifest = json(&env.join("rvbench.init.json"));
    let components = manifest["components"].as_array().unwrap();
    assert_eq!(components.len(), 1);
    assert_eq!(components[0]["component"], "worker");
}

#[test]
fn non_empty_directory_needs_force() {
    let temp = tempfile::tempdir().unwrap();
    let repos = temp.path().join("repos");
    fixtures(&repos);
    let env = temp.path().join("env");
    fs::create_dir_all(&env).unwrap();
    fs::write(env.join("keep.txt"), "occupied").unwrap();

    let refused = init(temp.path(), &repos, &["env", "--suite"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("非空"));
    assert_eq!(
        fs::read_to_string(env.join("keep.txt")).unwrap(),
        "occupied"
    );

    let forced = init(temp.path(), &repos, &["env", "--suite", "--force"]);
    assert!(
        forced.status.success(),
        "{}",
        String::from_utf8_lossy(&forced.stderr)
    );
    assert!(env.join("suites/kernels/hello-world/spec.json").is_file());
    assert!(env.join("keep.txt").is_file());
    assert!(!env.join("worker").exists());
}

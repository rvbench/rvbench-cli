# rvbench-cli

[`rvbench`](https://github.com/rvbench/rvbench) 的控制面客户端与离线合同校验库。Rust 原生、无 Python 包装，`--help` 查看全部子命令。

本仓自 2026-10-19 起从主仓的 `cli/` 目录拆出独立发布；主仓只保留维护工具 `rvbench-tools`，并以 git 依赖固定本仓 rev。

## 安装

```sh
cargo install --path . --locked     # 同时安装 rvbench 与 rb-cli
cargo build --release --locked      # 或只构建产物
```

`rb-cli` 是 `rvbench` 的命令别名：两个 bin 目标共用同一入口，参数、退出码与输出完全一致，`--help` 会显示实际被调用的名字。

```sh
rvbench --help
rb-cli --help        # 同一套子命令
```

## 控制面

`--base` 指向 manager（默认 `http://127.0.0.1:18800`）或独立 worker 的 18801；直连 worker 时加 `--direct`。

```sh
rvbench control workers                      # 节点
rvbench control capabilities                 # 能力表
rvbench control submit job.json --wait       # 提交
rvbench control get JOB_UUID --wait          # 查询
rvbench control cancel JOB_UUID              # 取消未运行任务
rvbench control drain WORKER_ID              # 排空节点
```

## 离线校验

不访问网络；合同 schema 在编译期嵌入，单个二进制即可校验任意文档。

```sh
rvbench validate spec examples/hello-world/spec.json
rvbench validate results examples/hello-world/eval_results.json --spec examples/hello-world/spec.json
rvbench validate baseline-results examples/baseline/baseline-results.json --spec examples/baseline/spec.json
rvbench digest examples/hello-world/spec.json
rvbench digest examples/baseline/spec.json --baseline torch_abs
```

`validate hidden` 针对评测集的 `spec.hidden.json`；公开仓不含隐藏定义，完整集随 `rvbench-official-suites` 开源。

`validate spec|hidden` 的 `--require-ready` 在仍有 readiness blocker 时返回 2；合同合法与完成板上验证是两件事。

## 库

`rvbench_cli` 同时是 lib，供 `rvbench-tools` 复用同一套校验与 I/O：

- `spec` / `results` / `baseline` / `hidden`：schema 与语义校验、readiness、摘要
- `files`：原子写入
- `control`：HTTP 客户端

`json`（严格解析、canonical 序列化、SHA-256）来自 `rvbench-protocol`，与 manager / worker 共用同一实现。

## 目录

```
rvbench-cli/
├── src/                  lib + bin 源码
├── tests/                合同回归（parity / baseline / commands / contracts）
├── contracts/            主仓合同副本：schemas 编译期嵌入，api/openapi.json 供逐字节比对
└── examples/             hello-world 与 baseline 冒烟样例
```

`contracts/` 与 `examples/` 是主仓的只读副本，正本在主仓 `contracts/` 与 `examples/`，用主仓的 `python tools/sync_cli.py <本仓检出>` 单向同步，不要在本仓直接改。

## 版本关系

- `rvbench-protocol` 以 git 依赖固定 rev；改协议须先推协议仓，再更新本仓 `Cargo.toml` 的 rev 与 `Cargo.lock`。
- 主仓 `rvbench-tools` 以 git 依赖固定本仓 rev；改 CLI 公共 API 后须在主仓同步 rev。

# contracts（副本）

本目录是主仓 [`rvbench`](https://github.com/rvbench/rvbench) `contracts/` 的**只读副本**，随本仓一起编译，保证离线构建与测试不依赖外部检出。

| 子目录 | 来源 | 用途 |
|---|---|---|
| `schemas/*.schema.json` | 主仓 `contracts/schemas/` | `rvbench-cli` 编译期通过 `include_str!` 嵌入，驱动 `validate` 子命令 |
| `api/openapi.json` | 主仓 `contracts/api/` | `tests/contracts.rs` 与 `rvbench-protocol::SPEC` 逐字节比对 |

## 规则

1. **正本只在主仓**。本目录不单独修改，改合同请回主仓改，然后跑主仓的 `python tools/sync_cli.py <本仓检出路径>` 同步。
2. `tests/contracts.rs` 同时守住两件事：副本与 `rvbench-protocol` 的 `SPEC` 一致；文件为 LF、无 CRLF。
3. 同步后若 `cargo test` 报 `rvbench-protocol::SPEC` 不一致，说明协议仓还没同步：先在主仓跑 `python tools/sync_contract.py`，再更新本仓 `Cargo.toml` 里的 `rvbench-protocol` rev。

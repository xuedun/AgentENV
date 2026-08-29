# 本地修改说明

本文档记录对 AgentENV 仓库的本地修改，区别于原始 README.md 的上游文档。

## 1. restore 耗时打桩（start_resume timing breakdown）

### 修改文件

- `src/sandbox/firecracker/sandbox.rs`

### 修改内容

在 `start_resume` 函数（`FirecrackerSandbox` trait impl）中，为 restore 全流程的 14 个关键节点添加 `std::time::Instant` 计时，在函数末尾用 `tracing::info!` 输出完整耗时拆分。

同时在文件头 imports 中将 `tracing::{debug, trace, warn}` 扩展为 `tracing::{debug, info, trace, warn}`。

### restore 代码路径

```
aenv start <template_id> -d
  → HTTP POST /sandboxes (orchestrator service.rs)
  → launch_sandbox(LaunchPlan::for_resume(...))
  → FirecrackerSandbox::start() → start_resume(config)
```

`start_resume` 内部步骤及对应计时字段：

| 序号 | 步骤 | 计时字段 | 说明 |
|---|---|---|---|
| 1 | ublk daemon 可用性检查 | `d_warm`（含 warm pool 获取） | 检查 ublk daemon client 是否就绪；尝试从 warm pool 获取预热的 firecracker 实例 |
| 2 | tools drive 符号链接 | `d_tools` | `link_tools_drive`：symlink rootfs.ext4 → tools drive |
| 3 | 创建 rootfs overlaybd ublk 设备 | `d_rootfs_ublk` | `UblkDeviceManager::create_overlaybd_runtime_device` → daemon RPC `CreateOverlaybdRuntimeDevice`：打开 overlaybd 镜像层栈 + 创建 ublk 块设备 + 符号链接到工作目录 |
| 4 | 额外磁盘准备 | `d_extra` | `prepare_snapshot_backing_drives`：为 snapshot 配置中的额外磁盘准备后端 |
| 5 | 网络命名空间 + firecracker 进程 | `d_net_spawn` | `NetworkManager::allocate_any()` 分配网络 slot + `fc_instance.spawn_with_netns()` 启动 firecracker 进程 + `slot.set_egress_policy()` 配置 iptables 规则。若使用了 warm pool 则此步跳过进程 spawn |
| 6 | custom extension hook | `d_hook` | 调用 `start_resume` hook（仅当 `[custom_extension].url` 配置时触发，否则耗时为 0） |
| 7 | 创建共享内存 ublk 设备 | `d_mem_ublk` | `UblkDeviceManager::get_or_create_shared_mem` → daemon RPC `AcquireOverlaybd(Shared)`：创建或复用内存快照的 overlaybd ublk 设备。首次创建慢（需打开层栈），复用快（引用计数 +1） |
| 8 | 等待 firecracker socket | `d_socket` | `fc_instance.wait_for_ready()`：轮询 firecracker API socket 就绪（仅冷启动时需要，warm pool 跳过） |
| 9 | 配置 firecracker logger | `d_logger` | `configure_logger()`：Firecracker API call（PUT /logger） |
| 10 | 加载快照 | `d_load` | `fc_instance.load_snapshot_file()`：Firecracker API call（PUT /snapshot/load），加载 vm_state.bin + 内存后端（file-backed ublk 设备） |
| 11 | 设置 MMDS 元数据 | `d_mmds` | `fc_instance.set_mmds()`：Firecracker API call（PUT /mmds/config） |
| 12 | 修补磁盘限速器 | `d_limiter` | `reconcile_disk_rate_limiter` + `fc_instance.patch_drive_rate_limiter()`：Firecracker API call（PATCH /drives/{id}），覆盖快照继承的限速配置 |
| 13 | firecracker 恢复 VM | `d_fc_resume` | `fc_instance.resume()`：Firecracker API call（PATCH /vm → Resumed），实际恢复 VM 执行 |

### 输出格式

server 日志（`RUST_LOG=info`）中，每个沙箱 resume 完成后输出一条结构化日志：

```
info: start_resume timing breakdown
  sandbox_id=01a04b39-...
  d_total_ms=850
  d_warm_ms=2
  d_tools_ms=1
  d_rootfs_ublk_ms=320
  d_extra_ms=5
  d_net_spawn_ms=45
  d_hook_ms=0
  d_mem_ublk_ms=180
  d_socket_ms=80
  d_logger_ms=3
  d_load_ms=150
  d_mmds_ms=5
  d_limiter_ms=2
  d_fc_resume_ms=57
```

### 预期瓶颈

| 步骤 | 字段 | 预期耗时 | 原因 |
|---|---|---|---|
| rootfs ublk 创建 | `d_rootfs_ublk` | 高（100-500ms） | daemon RPC → overlaybd 镜像层栈打开（多层 local/registry 后端） + ublk 设备创建（io_uring ADD） |
| 内存 ublk 创建 | `d_mem_ublk` | 中-高（50-300ms） | daemon RPC → 共享内存 ublk 设备。首次创建需打开层栈，复用时仅引用计数 +1（<1ms） |
| 网络命名空间+进程 | `d_net_spawn` | 中（20-100ms） | firecracker 进程 spawn + netns 创建 + veth + iptables 规则。warm pool 时跳过 |
| 等 socket 就绪 | `d_socket` | 中（10-100ms） | 轮询 firecracker Unix socket。warm pool 时跳过 |
| 加载快照 | `d_load` | 中（50-200ms） | Firecracker PUT /snapshot/load：读取 vm_state.bin + 设置 file-backed 内存后端 |
| VM 恢复 | `d_fc_resume` | 低-中（10-60ms） | Firecracker PATCH /vm → Resumed：恢复 KVM vCPU 线程执行 |

### 使用方式

```bash
# 编译
cd /home/cxd/AgentENV
cargo build --release -p agentenv

# 启动 server（info 级别日志）
sudo -E API_ADDR=0.0.0.0:8000 AENV_RUN_USER=cxd RUST_LOG=info \
    ./target/release/server > /tmp/server.log 2>&1 &

# 执行 restore 后查看耗时
grep "start_resume timing" /tmp/server.log
```

### 相关代码位置

- 打桩函数：`src/sandbox/firecracker/sandbox.rs` → `start_resume`（约 line 1372）
- ublk daemon RPC（rootfs）：`src/sandbox/ublk/device.rs:355` → `create_overlaybd_runtime_device`
- ublk daemon RPC（memory）：`src/sandbox/ublk/device.rs:470` → `get_or_create_shared_mem`
- firecracker API calls：`src/sandbox/firecracker/instance.rs` → `load_snapshot_file`（line 512）、`resume`（line 445）
- orchestrator 调度：`src/orchestrator/service.rs` → `resume_sandbox`（line 1276）、`launch_sandbox`

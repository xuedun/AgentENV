# 本地修改说明

本文档记录对 AgentENV 仓库的本地修改，区别于原始 README.md 的上游文档。

## 1. restore 耗时打桩

### 目标

对沙箱 restore（从快照/模板恢复）的完整 server 侧路径打桩，使各阶段耗时之和 ≈ 总耗时，便于定位瓶颈。

### 修改文件

| 文件 | 修改内容 |
|---|---|
| `src/orchestrator/service.rs` | `launch_sandbox` 函数加 7 阶段计时 + `Instant` import |
| `src/sandbox/firecracker/sandbox.rs` | `start_resume` 函数加 14 步计时 + `info` import |

### 打桩层级

打桩分两层，外层覆盖完整 server 侧路径，内层是 `start_nowait` 的细粒度拆分：

```
replay 测量的 restore_time
  = aenv CLI 进程启动 + HTTP 往返 + server 侧 launch_sandbox + find_firecracker_pid
                                    ↑ 打桩覆盖范围

┌─ launch_sandbox (service.rs) ──────────────────────────────────────────────┐
│                                                                            │
│  1. d_build         build_sandbox: factory.build_from_paused_state         │
│  2. d_protect       protect_image_refs                                     │
│  3. d_start_nowait  sandbox.start_nowait() ──┐                            │
│  4. d_register     store handle + release_image_refs                      │
│  5. d_persist       store.add (metadata 持久化)                             │
│  6. d_wait_ready    sandbox.wait_for_ready() (等 envd 就绪)                │
│  7. d_finalize      update_if_state + proxy route + delete record         │
│                                                                            │
│  d_total = d_build + d_protect + d_start_nowait + d_register              │
│          + d_persist + d_wait_ready + d_finalize                           │
└────────────────────────────────────────────────────────────────────────────┘
                    │
                    ▼ d_start_nowait 内部展开
┌─ start_resume (sandbox.rs) ────────────────────────────────────────────────┐
│                                                                            │
│  1. d_warm          warm pool 获取 firecracker (有 pool 则快)              │
│  2. d_tools         tools drive 符号链接                                   │
│  3. d_rootfs_ublk   ★ 创建 rootfs overlaybd ublk 设备 (daemon RPC)        │
│  4. d_extra         额外磁盘准备                                           │
│  5. d_net_spawn     ★ 网络命名空间 + firecracker 进程 spawn                │
│  6. d_hook          custom extension hook (未配置则为 0)                   │
│  7. d_mem_ublk      ★ 创建共享内存 ublk 设备 (daemon RPC, 复用时快)        │
│  8. d_socket        ★ 等待 firecracker API socket 就绪 (冷启动慢)         │
│  9. d_logger        配置 firecracker logger (API call)                     │
│ 10. d_load          ★ 加载快照: PUT /snapshot/load (VM 状态+内存后端)      │
│ 11. d_mmds          设置 MMDS 元数据 (API call)                             │
│ 12. d_limiter       修补磁盘限速器 (API call)                               │
│ 13. d_fc_resume     ★ firecracker 恢复 VM: PATCH /vm → Resumed           │
│                                                                            │
│  d_total = d_warm + d_tools + d_rootfs_ublk + d_extra + d_net_spawn        │
│          + d_hook + d_mem_ublk + d_socket + d_logger + d_load             │
│          + d_mmds + d_limiter + d_fc_resume                                │
└────────────────────────────────────────────────────────────────────────────┘
```

### 日志输出

server 日志（`RUST_LOG=info`）中，每个沙箱 restore 完成后输出两条结构化日志：

**外层 — launch_sandbox（覆盖完整 server 侧路径）：**

```
launch_sandbox timing breakdown
  sandbox_id=01a04c7c-...
  d_total_ms=90
  d_build_ms=1
  d_protect_ms=0
  d_start_nowait_ms=12    ← 对应 start_resume 内层 d_total
  d_register_ms=2
  d_persist_ms=1
  d_wait_ready_ms=65       ← 等 envd 就绪（通常是最大瓶颈）
  d_finalize_ms=9
```

**内层 — start_resume（start_nowait 的细粒度拆分）：**

```
start_resume timing breakdown
  sandbox_id=01a04c7c-...
  d_total_ms=12            ← 应 ≈ 外层 d_start_nowait_ms
  d_warm_ms=0
  d_tools_ms=0
  d_rootfs_ublk_ms=2
  d_extra_ms=0
  d_net_spawn_ms=0         ← warm pool 时为 0
  d_hook_ms=0
  d_mem_ublk_ms=0          ← 复用共享内存设备时为 0
  d_socket_ms=0            ← warm pool 时为 0
  d_logger_ms=0
  d_load_ms=9              ← Firecracker PUT /snapshot/load
  d_mmds_ms=0
  d_limiter_ms=0
  d_fc_resume_ms=0
```

### 预期瓶颈

| 阶段 | 字段 | 预期耗时 | 原因 |
|---|---|---|---|
| build sandbox | `d_build` | 低（1-5ms） | 构造 FirecrackerSandbox 结构体，读 snapshot config |
| start_nowait | `d_start_nowait` | 中（10-50ms） | 整个 start_resume（见内层拆分） |
| rootfs ublk 创建 | `d_rootfs_ublk` | 高（100-500ms） | daemon RPC → overlaybd 镜像层栈打开 + ublk 设备创建 |
| 内存 ublk 创建 | `d_mem_ublk` | 中-高（50-300ms） | daemon RPC → 共享内存 ublk。首次创建慢，复用快（<1ms） |
| 网络命名空间+进程 | `d_net_spawn` | 中（20-100ms） | firecracker spawn + netns + veth + iptables。warm pool 时为 0 |
| 等 socket | `d_socket` | 中（10-100ms） | 轮询 firecracker socket。warm pool 时为 0 |
| 加载快照 | `d_load` | 中（50-200ms） | Firecracker PUT /snapshot/load：vm_state.bin + file 内存后端 |
| **等 envd 就绪** | **`d_wait_ready`** | **高（50-500ms）** | **轮询 envd HTTP 健康检查，等 VM 内 envd 进程启动** |
| finalize | `d_finalize` | 低（5-20ms） | 状态更新 + proxy route + 删除持久化记录 |

### 使用方式

```bash
# 编译
cd /home/cxd/AgentENV
make build-server-release
# 或
cargo build --release -p agentenv --bin server

# 启动 server（info 级别日志）
sudo -E API_ADDR=0.0.0.0:8000 AENV_RUN_USER=cxd RUST_LOG=info \
    ./target/release/server > /tmp/server.log 2>&1 &

# 执行 restore 后查看耗时
grep "launch_sandbox timing" /tmp/server.log   # 外层
grep "start_resume timing" /tmp/server.log      # 内层
```

### 验证各阶段之和 ≈ 总耗时

```bash
# 提取单次 restore 的各阶段耗时（ms），验证求和
grep "launch_sandbox timing" /tmp/server.log | tail -1
# d_total 应 ≈ d_build + d_protect + d_start_nowait + d_register + d_persist + d_wait_ready + d_finalize

grep "start_resume timing" /tmp/server.log | tail -1
# d_total 应 ≈ d_warm + d_tools + d_rootfs_ublk + d_extra + d_net_spawn + d_hook + d_mem_ublk + d_socket + d_logger + d_load + d_mmds + d_limiter + d_fc_resume
```

### 相关代码位置

| 文件 | 函数 | 行号 |
|---|---|---|
| `src/orchestrator/service.rs` | `launch_sandbox`（7 阶段打桩） | ~1972 |
| `src/sandbox/firecracker/sandbox.rs` | `start_resume`（14 步打桩） | ~1372 |
| `src/sandbox/firecracker/sandbox.rs` | `start_nowait`（路由到 start_resume） | ~585 |
| `src/sandbox/firecracker/sandbox.rs` | `wait_for_ready`（envd 就绪等待） | ~598 |
| `src/sandbox/ublk/device.rs` | `create_overlaybd_runtime_device`（rootfs ublk RPC） | ~355 |
| `src/sandbox/ublk/device.rs` | `get_or_create_shared_mem`（memory ublk RPC） | ~470 |
| `src/sandbox/firecracker/instance.rs` | `load_snapshot_file`（Firecracker API） | ~512 |
| `src/sandbox/firecracker/instance.rs` | `resume`（Firecracker API） | ~445 |

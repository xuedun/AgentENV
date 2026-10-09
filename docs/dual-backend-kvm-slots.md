# 双后端 KVM Slot 限制说明

## 问题背景

双后端内存快照（dual-backend）将 VM 内存分为 base 和 delta 两个 ublk 设备：
- **base 设备**：只读，共享自模板，包含未被脏页覆盖的内存页
- **delta 设备**：可写，包含 pause 时捕获的脏页

resume 时，`compute_file_offset_ranges` 遍历 delta 层索引，生成 base/delta 交替的范围列表，通过 Firecracker 的 `load_snapshot_multi_backend` 将每个范围注册为一个 KVM memory slot。

当脏页分布碎片化时，base 和 delta 范围频繁交替，slot 数量可能较大。

## KVM Slot 限制现状

### 旧内核（< 6.0）

旧内核在 UAPI 头文件中定义了固定上限：

```c
// arch/x86/include/uapi/asm/kvm.h（旧）
#define KVM_NR_MEM_SLOTS      512
#define KVM_USER_MEM_SLOTS    509   // 512 - 3
```

509 = 512 - 3，其中 3 个 slot 保留给 vCPU TSS、APIC 等特殊用途。

### 本机内核 6.6.0-efr20-gcc30+

本机内核 **没有 509 限制**。源码路径：

```
/home/qcm/kern/efr-clean/
```

相关定义在 `include/linux/kvm_host.h`：

```c
// include/linux/kvm_host.h 第 684-689 行
#ifndef KVM_INTERNAL_MEM_SLOTS
#define KVM_INTERNAL_MEM_SLOTS 0          // ARM64 默认为 0
#endif

#define KVM_MEM_SLOTS_NUM    SHRT_MAX     // = 32767
#define KVM_USER_MEM_SLOTS   (KVM_MEM_SLOTS_NUM - KVM_INTERNAL_MEM_SLOTS)
                                         // = 32767 - 0 = 32767
```

UAPI 头文件（`include/uapi/linux/kvm.h`）中 **已无 `KVM_NR_MEM_SLOTS` 常量**。

运行时检查在 `virt/kvm/kvm_main.c` 第 2084 行：

```c
if ((u16)mem->slot >= KVM_USER_MEM_SLOTS)
    return -EINVAL;
```

**实际限制为 32767，不需要修改内核。**

### 验证方法

```bash
# 确认内核源码路径
readlink /lib/modules/$(uname -r)/build
# → /home/qcm/kern/efr-clean

# 查看 slot 上限定义
grep -n "KVM_MEM_SLOTS_NUM\|KVM_USER_MEM_SLOTS\|KVM_INTERNAL_MEM_SLOTS" \
  /home/qcm/kern/efr-clean/include/linux/kvm_host.h
```

## AgentENV 侧的限制

之前双后端失败的真正原因是 **AgentENV 自己的检查过于保守**：

```rust
// src/sandbox/firecracker/sandbox.rs — setup_dual_backend() 中
// 旧值：509（旧内核遗留，过于保守）
// 新值：32767（匹配内核 6.6 的 KVM_USER_MEM_SLOTS = SHRT_MAX）
const MAX_KVM_SLOTS: usize = 32767;
if ranges.len() > MAX_KVM_SLOTS {
    anyhow::bail!(
        "dual-backend ranges {} exceed KVM slot limit {}",
        ranges.len(),
        MAX_KVM_SLOTS
    );
}
```

测试中 1247 个范围 > 509，触发了 AgentENV 自己的 bail，而非内核拒绝。

### 修复方法

将 `MAX_KVM_SLOTS` 改为与内核匹配的值：

```rust
const MAX_KVM_SLOTS: usize = 32767;  // 匹配内核 6.6 的 KVM_USER_MEM_SLOTS
```

代码位置：`src/sandbox/firecracker/sandbox.rs`，`setup_dual_backend` 函数内，约第 1644 行。

### 变更记录

| 日期 | 旧值 | 新值 | 原因 |
|------|------|------|------|
| 2026-09-30 | 509 | 32767 | 内核 6.6 已将 slot 上限改为 SHRT_MAX，509 是旧内核遗留的保守值 |

## 如果需要修改内核限制（其他环境）

如果在不支持 32767 slot 的旧内核上运行，需要修改内核源码：

### 源码位置

```
内核源码树：/home/qcm/kern/efr-clean/
修改文件：include/linux/kvm_host.h 第 688 行
```

### 修改内容

```c
// 修改前
#define KVM_MEM_SLOTS_NUM SHRT_MAX

// 修改后（如果需要更大的值，SHRT_MAX 已经是 32767）
// SHRT_MAX 通常是 32767，已经足够双后端使用
// 如果确实需要更大，可改为固定值
#define KVM_MEM_SLOTS_NUM 65536
```

同时检查 `virt/kvm/kvm_main.c` 中的 `BUILD_BUG_ON(KVM_MEM_SLOTS_NUM > SHRT_MAX)`，
如果改为超过 SHRT_MAX 的值，需要删除或修改此断言。

### 重新编译

```bash
cd /home/qcm/kern/efr-clean
make -j$(nproc)
make modules_install
make install
reboot
```

## 注意事项

- 本机内核 6.6.0-efr20-gcc30+ 的 slot 上限已是 32767，无需修改内核
- 只需修改 AgentENV 的 `MAX_KVM_SLOTS` 常量
- 每个 slot 约占用 64-128 字节内核内存，32767 个 slot 的额外内存开销约 2-4MB，可忽略
- Firecracker 不额外限制 slot 数量，直接透传给 KVM ioctl
- `compute_file_offset_ranges` 已移除 `MIN_BASE_GAP` 合并逻辑，保持 base/delta 精准交替

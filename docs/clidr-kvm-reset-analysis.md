# KVM CLIDR_EL1 Reset 问题分析：Debian 6.6 vs openEuler 6.6

## 1. 问题描述

在 ARM 平台上使用 Firecracker 启动 2U4G 沙箱（2 vCPU）时：

- **Debian 6.6 内核**：Firecracker 只能使用其中一个 vCPU，第二个 vCPU 无法上线。
- **openEuler 6.6 内核**：两个 vCPU 均正常工作。

经分析定位为 KVM 在 reset CLIDR_EL1 寄存器时存在问题，导致第二个 vCPU 无法被 guest 内核正确启动。

## 2. 对比环境

| 项目 | Debian | openEuler |
|------|--------|-----------|
| 内核版本 | Linux 6.6.158（stable 分支最新） | OLK-6.6（基于 6.6） |
| 源码来源 | `git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git` | `gitee.com/openeuler/kernel` |
| 关键文件 | `arch/arm64/kvm/sys_regs.c` | 同左 |
| 对比行数 | 3660 行 | 4085 行 |

## 3. 根因分析

### 3.1 问题本质

CLIDR_EL1（Cache Level ID Register）描述缓存拓扑结构。KVM 对其进行 trap-and-emulate：
- guest 访问 CLIDR_EL1 时触发 trap（HCR_EL2.TID2）
- KVM 通过 `access_clidr()` 返回 `__vcpu_sys_reg(vcpu, CLIDR_EL1)` 中存储的值
- 该值由 `reset_clidr()` 在 vCPU reset 时初始化

**核心问题**：Debian 6.6 在**每次** vCPU reset 时都重新调用 `reset_clidr()`，覆盖了 userspace（Firecracker）先前通过 `KVM_SET_ONE_REG` 设定的值。当 PSCI CPU_ON 唤醒第二个 vCPU 时，CLIDR 被重置为 KVM 伪造的默认值，guest 内核因 CLIDR 值不符预期而启动失败。

### 3.2 对应主线修复

openEuler 合入了主线 commit：

- **commit**：`e016333`（"KVM: arm64: Only reset vCPU-scoped feature ID regs once"）
- **作者**：Oliver Upton（oupton）
- **合入者**：Marc Zyngier
- **日期**：2024 年 5 月 2 日
- **目标版本**：Linux 6.10 开发周期
- **回传状态**：**未回传到 6.6.y stable 分支**

主线 commit 描述明确指出：

> "The general expectation with feature ID registers is that they're 'reset' exactly once by KVM for the lifetime of a vCPU/VM, such that any [value set by userspace is preserved]."

> "Fixes for CLIDR_EL1 and MPIDR_EL1 being accidentally mutable across a vCPU reset."

## 4. 代码对比

### 4.1 `reset_clidr()` 函数（两端完全一致）

两个内核的 `reset_clidr()` 实现完全相同，都基于 CTR_EL0 伪造 CLIDR 值：

```c
// 两端一致 — sys_regs.c
static u64 reset_clidr(struct kvm_vcpu *vcpu, const struct sys_reg_desc *r)
{
    u64 ctr_el0 = read_sanitised_ftr_reg(SYS_CTR_EL0);
    u64 clidr;
    u8 loc;

    if ((ctr_el0 & CTR_EL0_IDC)) {
        loc = (ctr_el0 & CTR_EL0_DIC) ? 1 : 2;
        clidr = CACHE_TYPE_UNIFIED << CLIDR_CTYPE_SHIFT(loc);
    } else {
        loc = 1;
        clidr = 1 << CLIDR_LOUU_SHIFT;
        clidr |= 1 << CLIDR_LOUIS_SHIFT;
        clidr |= CACHE_TYPE_DATA << CLIDR_CTYPE_SHIFT(1);
    }

    if (!(ctr_el0 & CTR_EL0_DIC))
        clidr |= CACHE_TYPE_INST << CLIDR_CTYPE_SHIFT(1);

    clidr |= loc << CLIDR_LOC_SHIFT;

    if (kvm_has_mte(vcpu->kvm))
        clidr |= 2ULL << CLIDR_TTYPE_SHIFT(loc);

    __vcpu_sys_reg(vcpu, r->reg) = clidr;
    return __vcpu_sys_reg(vcpu, r->reg);
}
```

函数本身不是问题所在，问题在于**调用时机**。

### 4.2 `kvm_reset_sys_regs()` — 核心差异

#### Debian 6.6（有 Bug）

```c
// debian-6.6/arch/arm64/kvm/sys_regs.c:3207
void kvm_reset_sys_regs(struct kvm_vcpu *vcpu)
{
    unsigned long i;

    kvm_reset_id_regs(vcpu);   // 先初始化 ID 寄存器（仅一次）

    for (i = 0; i < ARRAY_SIZE(sys_reg_descs); i++) {
        const struct sys_reg_desc *r = &sys_reg_descs[i];

        if (is_id_reg(reg_to_encoding(r)))   // is_id_reg 只认 Op1==0
            continue;                        // CLIDR(Op1=1) 不匹配，不跳过

        if (r->reset)
            r->reset(vcpu, r);  // ← reset_clidr 每次都被调用！
    }
}
```

`is_id_reg()` 定义：

```c
// Debian — 只认 Op1==0 的标准 ID 寄存器
static inline bool is_id_reg(u32 id)
{
    return (sys_reg_Op0(id) == 3 && sys_reg_Op1(id) == 0 &&
            sys_reg_CRn(id) == 0 && sys_reg_CRm(id) >= 1 &&
            sys_reg_CRm(id) < 8);
}
```

CLIDR_EL1 编码为 Op0=3, Op1=1, CRn=0, CRm=0, Op2=1。由于 Op1=1，`is_id_reg()` 返回 false，CLIDR 被当作普通寄存器处理，在**每次** `kvm_reset_sys_regs()` 调用时都执行 `reset_clidr()`。

#### openEuler 6.6（已修复）

```c
// openeuler-6.6/arch/arm64/kvm/sys_regs.c:3657
void kvm_reset_sys_regs(struct kvm_vcpu *vcpu)
{
    struct kvm *kvm = vcpu->kvm;
    unsigned long i;

    for (i = 0; i < ARRAY_SIZE(sys_reg_descs); i++) {
        const struct sys_reg_desc *r = &sys_reg_descs[i];

        if (!r->reset)
            continue;

        if (is_vm_ftr_id_reg(reg_to_encoding(r)))
            reset_vm_ftr_id_reg(vcpu, r);      // VM 级：每 VM 只 reset 一次
        else if (is_vcpu_ftr_id_reg(reg_to_encoding(r)))
            reset_vcpu_ftr_id_reg(vcpu, r);    // vCPU 级：每 vCPU 只 reset 一次 ← CLIDR 走这里
        else
            r->reset(vcpu, r);                 // 普通寄存器：每次 reset
    }

    set_bit(KVM_ARCH_FLAG_ID_REGS_INITIALIZED, &kvm->arch.flags);
}
```

CLIDR_EL1 被分类为 vCPU 级 feature ID 寄存器，通过 `reset_vcpu_ftr_id_reg()` 处理：

```c
// openEuler — 带初始化守卫
static void reset_vcpu_ftr_id_reg(struct kvm_vcpu *vcpu,
                                  const struct sys_reg_desc *reg)
{
    if (kvm_vcpu_initialized(vcpu))
        return;   // ← vCPU 已初始化后跳过，保留 userspace 设定的值

    reg->reset(vcpu, reg);
}
```

`kvm_vcpu_initialized()` 检查 `VCPU_INITIALIZED` flag，该 flag 在 `__kvm_vcpu_set_target()` 中于 `kvm_reset_vcpu()` **之后**设置：

```c
// arm.c — VCPU_INITIALIZED 在 reset 之后设置
ret = kvm_reset_vcpu(vcpu);     // 第一次：reset 执行
...
vcpu_set_flag(vcpu, VCPU_INITIALIZED);  // 设置 flag
```

因此：
- **首次** `KVM_ARM_VCPU_INIT`：`VCPU_INITIALIZED` 未设置 → `reset_clidr()` 执行
- **后续** reset（如 PSCI CPU_ON）：`VCPU_INITIALIZED` 已设置 → `reset_clidr()` **跳过**

### 4.3 CLIDR_EL1 分类逻辑差异

| 判定函数 | Debian `is_id_reg()` | openEuler `is_feature_id_reg()` |
|---------|----------------------|--------------------------------|
| Op0 | == 3 | == 3 |
| Op1 | == 0 | < 2 **或** == 3 |
| CRn | == 0 | == 0 |
| CRm | [1, 8) | <= 7 |

CLIDR_EL1（Op0=3, Op1=1, CRn=0, CRm=0, Op2=1）：
- Debian：Op1=1 ≠ 0 → **不是** ID 寄存器 → 每次都 reset
- openEuler：Op1=1 < 2 → **是** feature ID 寄存器 → 只 reset 一次

```c
// openEuler — is_feature_id_reg 覆盖了 CLIDR
static bool is_feature_id_reg(u32 encoding)
{
    return (sys_reg_Op0(encoding) == 3 &&
            (sys_reg_Op1(encoding) < 2 || sys_reg_Op1(encoding) == 3) &&
            sys_reg_CRn(encoding) == 0 &&
            sys_reg_CRm(encoding) <= 7);
}
```

openEuler 进一步细分为 VM 级和 vCPU 级：

```c
// openEuler — VM 级 feature ID 寄存器
static inline bool is_vm_ftr_id_reg(u32 id)
{
    switch (id) {
    case SYS_CTR_EL0:    // CTR_EL0 提升为 VM 级
    case SYS_MIDR_EL1:
    case SYS_REVIDR_EL1:
    case SYS_AIDR_EL1:
        return true;
    default:
        return (sys_reg_Op0(id) == 3 && sys_reg_Op1(id) == 0 &&
                sys_reg_CRn(id) == 0 && sys_reg_CRm(id) >= 1 &&
                sys_reg_CRm(id) < 8);
    }
}

// openEuler — vCPU 级 feature ID 寄存器
static inline bool is_vcpu_ftr_id_reg(u32 id)
{
    return is_feature_id_reg(id) && !is_vm_ftr_id_reg(id);
    // CLIDR 是 feature ID 但不是 VM 级 → vCPU 级
}
```

### 4.4 系统寄存器表项差异

```c
// Debian — 无 .val 字段
{ SYS_DESC(SYS_CLIDR_EL1), access_clidr, reset_clidr, CLIDR_EL1,
  .set_user = set_clidr },

// openEuler — 增加 .val = ~CLIDR_EL1_RES0
{ SYS_DESC(SYS_CLIDR_EL1), access_clidr, reset_clidr, CLIDR_EL1,
  .set_user = set_clidr, .val = ~CLIDR_EL1_RES0 },
```

`.val = ~CLIDR_EL1_RES0` 表示 CLIDR_EL1 的 bits [46:0] 为可写位（bits [63:47] 为 RES0）。该值用于：
- `arm64_check_features()` 中作为 `writable_mask` 校验 userspace 写入值
- `kvm_vm_ioctl_get_reg_writable_masks()` 导出给 userspace 查询

### 4.5 HCR_EL2 Trap 配置差异

```c
// Debian — 仅检查 EVT 和 cache type 一致性
if (cpus_have_final_cap(ARM64_HAS_EVT) &&
    !cpus_have_final_cap(ARM64_MISMATCHED_CACHE_TYPE))
    vcpu->arch.hcr_el2 |= HCR_TID4;   // CLIDR 不 trap，guest 读物理值
else
    vcpu->arch.hcr_el2 |= HCR_TID2;   // CLIDR trap，KVM 模拟

// openEuler — 额外检查 VM 的 CTR_EL0 是否与 host 一致
if (cpus_have_final_cap(ARM64_HAS_EVT) &&
    !cpus_have_final_cap(ARM64_MISMATCHED_CACHE_TYPE) &&
    kvm_read_vm_id_reg(vcpu->kvm, SYS_CTR_EL0) == read_sanitised_ftr_reg(SYS_CTR_EL0))
    vcpu->arch.hcr_el2 |= HCR_TID4;   // CLIDR 不 trap
else
    vcpu->arch.hcr_el2 |= HCR_TID2;   // CLIDR trap，KVM 模拟
```

| Trap 位 | 效果 |
|---------|------|
| HCR_TID2 | Trap CLIDR_EL1、CTR_EL0、CCSIDR_EL1、CSSELR_EL1 → KVM 模拟 |
| HCR_TID4 | Trap 部分 ID 寄存器，但**不** trap CLIDR_EL1 → guest 读物理值 |

openEuler 的额外检查确保：当 VM 的 CTR_EL0 被 userspace 修改后，回退到 TID2 模式（CLIDR 被 trap 并由 KVM 模拟），避免 guest 看到不一致的缓存拓扑。

## 5. 其他 openEuler 改进

| 差异点 | Debian 6.6 | openEuler 6.6 |
|--------|-----------|---------------|
| CTR_EL0 处理 | invariant 寄存器（不可改） | VM 级 feature ID 寄存器（userspace 可通过 `KVM_SET_ONE_REG` 修改） |
| CTR_EL0 表项 | 在 `invariant_sys_regs[]` 中 | 在 `sys_reg_descs[]` 中，使用 `ID_WRITABLE` 宏，可写掩码为 `DIC\|IDC\|DminLine\|L1Ip\|IminLine` |
| 可写掩码 ioctl | 无 | `KVM_ARM_GET_REG_WRITABLE_MASKS`（导出 feature ID 寄存器可写位） |
| `kvm_reset_id_regs()` | 独立函数，先于主循环调用 | 取消，VM 级 ID 寄存器在主循环中通过 `reset_vm_ftr_id_reg()` 处理 |
| `invariant_sys_regs[]` | 存在 | 取消，改为 VM 级 feature ID 寄存器 |
| `lockdep_assert_held` | `kvm_reset_id_regs` 中有 | 移除（重构后不需要） |
| Realm/CVM 支持 | 无 | 有（`kvm_is_realm()`、REC 等） |

## 6. 问题复现路径

### Debian 6.6（问题复现）

```
1. KVM_CREATE_VCPU(vcpu0)         → vcpu0 创建，CLIDR=0
2. KVM_ARM_VCPU_INIT(vcpu0)       → kvm_reset_vcpu() → kvm_reset_sys_regs()
                                    → reset_clidr() 执行（VCPU_INITIALIZED 未设置）
                                    → CLIDR = 伪造值
                                    → VCPU_INITIALIZED 设置
3. KVM_SET_ONE_REG(vcpu0, CLIDR)  → set_clidr() → CLIDR = Firecracker 设定值
4. KVM_RUN(vcpu0)                 → guest 启动，读取 CLIDR（Firecracker 值）→ 正常

5. KVM_CREATE_VCPU(vcpu1)         → vcpu1 创建，CLIDR=0
6. KVM_ARM_VCPU_INIT(vcpu1)       → kvm_reset_vcpu() → kvm_reset_sys_regs()
                                    → reset_clidr() 执行（VCPU_INITIALIZED 未设置）
                                    → CLIDR = 伪造值
                                    → VCPU_INITIALIZED 设置
7. KVM_SET_ONE_REG(vcpu1, CLIDR)  → set_clidr() → CLIDR = Firecracker 设定值
8. KVM_RUN(vcpu1)                 → guest 通过 PSCI CPU_ON 启动 secondary CPU
                                    → KVM_REQ_VCPU_RESET → kvm_reset_vcpu()
                                    → kvm_reset_sys_regs()
                                    → reset_clidr() 再次执行！← 覆盖 Firecracker 值
                                    → CLIDR = 伪造值（与 guest 预期不符）
                                    → secondary CPU 启动失败 ✗
```

### openEuler 6.6（正常工作）

```
1-4. 同 Debian，vcpu0 正常启动

5. KVM_CREATE_VCPU(vcpu1)         → vcpu1 创建，CLIDR=0
6. KVM_ARM_VCPU_INIT(vcpu1)       → kvm_reset_vcpu() → kvm_reset_sys_regs()
                                    → reset_vcpu_ftr_id_reg()
                                    → kvm_vcpu_initialized() = false → reset_clidr() 执行
                                    → CLIDR = 伪造值
                                    → VCPU_INITIALIZED 设置
7. KVM_SET_ONE_REG(vcpu1, CLIDR)  → set_clidr() → CLIDR = Firecracker 设定值
8. KVM_RUN(vcpu1)                 → guest 通过 PSCI CPU_ON 启动 secondary CPU
                                    → KVM_REQ_VCPU_RESET → kvm_reset_vcpu()
                                    → kvm_reset_sys_regs()
                                    → reset_vcpu_ftr_id_reg()
                                    → kvm_vcpu_initialized() = true → 跳过 reset_clidr()
                                    → CLIDR 保持 Firecracker 设定值 ✓
                                    → secondary CPU 启动成功 ✓
```

## 7. 修复状态总结

| 项目 | 状态 |
|------|------|
| 主线 commit | `e016333`（"KVM: arm64: Only reset vCPU-scoped feature ID regs once"） |
| 合入主线版本 | Linux 6.10 开发周期（2024 年 5 月） |
| 回传 6.6.y stable | **未回传** |
| Debian 6.6.158 | **存在 Bug** |
| openEuler OLK-6.6 | **已修复**（合入主线修复 + 重构 ID 寄存器管理框架） |
| 关联 commit | `606af82`（selftests：验证 CLIDR_EL1 和 MPIDR_EL1 在 vCPU reset 后值保持不变） |

## 8. 结论

1. **Debian 6.6 确实存在该问题**：`reset_clidr()` 在每次 vCPU reset 时都被调用，覆盖 userspace 设定的 CLIDR_EL1 值，导致第二个 vCPU 无法正常上线。

2. **openEuler 6.6 已解决该问题**：通过将 CLIDR_EL1 分类为 vCPU 级 feature ID 寄存器并加入 `kvm_vcpu_initialized()` 守卫，确保 `reset_clidr()` 只在首次初始化时调用一次，后续 reset 保留 userspace 设定的值。

3. **根因是 CLIDR 调用时机，而非函数本身**：两个内核的 `reset_clidr()` 函数实现完全一致，差异在于调用策略——Debian 每次都调用，openEuler 只调用一次。

4. **修复未回传 stable**：主线修复在 6.10 周期合入，未回传 6.6.y stable 分支。如需在 Debian 6.6 上修复，需要手动 backport commit `e016333` 及相关改动。

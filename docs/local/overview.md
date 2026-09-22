# AgentENV 存储与内存快照主干脉络

本目录用于梳理 AgentENV 中从 overlaybd 镜像格式到 Firecracker VM 内存恢复的完整链路。
所有代码引用基于 `/home/cxd/AgentENV`。

## 文档索引

| 文档 | 内容 |
|------|------|
| [image-file-open-flow.md](./image-file-open-flow.md) | image.json → ImageFile 的打开与多层 stack 流程 |

---

## 核心概念速览

AgentENV 的存储子系统有两条数据通路，共用 overlaybd + ublk 底座：

```
┌─────────────────────────────────────────────────────────────┐
│  块设备通路（rootfs / 额外磁盘）                              │
│    OCI 镜像层 → overlaybd commit 文件 → ublk /dev/ublkbN    │
│    → Firecracker 挂为 /dev/vda，guest 内部挂 ext4           │
├─────────────────────────────────────────────────────────────┤
│  内存快照通路（VM 物理内存恢复）                              │
│    pause 时读脏页 → overlaybd 内存层 commit 文件             │
│    resume 时多层 stack → 只读 ublk /dev/ublkbM              │
│    → Firecracker mmap MAP_PRIVATE 作为 guest 物理内存后端    │
│    写时 COW 到匿名页，不污染只读层                             │
└─────────────────────────────────────────────────────────────┘
```

两条通路的底层格式完全一致：都是 overlaybd LSMT commit 文件 + image.json 配置 + ImageFile 对象 + OverlaybdTarget + ublk 设备。
区别只在：

- **块设备通路**：ImageFile 有可写 upper（log-structured append），guest 写直接落到 rootfs。
- **内存快照通路**：ImageFile 纯只读（lower stack），Firecracker 用 mmap 而非块设备挂载，写靠内核 COW。

## 从磁盘到 VM 的关键阶段

```
① commit 文件（磁盘）
   .overlaybd.commit
   可能被 ZFile 压缩包裹（magic "ZFile\0\x01\0"）
   内部是 LSMT 格式（magic "LSMT\0\1\2"）：
     Header(4K) + Data 区 + Index 区(DiskSegmentMapping[16B]) + Trailer(4K)
   详见 overlaybd-commit-format.md §2-§3

② image.json 配置（磁盘）
   声明 lowers（commit 文件路径列表，bottom-to-top）+ upper（可写层路径）
   详见 image-file-open-flow.md

③ ImageFile（内存对象）
   打开 image.json → 打开每个 commit → merge 各层 index → 一个 ImageFile
   持有 Vec<VirtualFile>（各 commit 文件）+ ReadOnlyIndex（合并索引）
   详见 image-file-open-flow.md

④ OverlaybdTarget（内存对象）
   持有 Arc<ImageFile> + dev_sectors + block_size_shift
   实现 UVMUblkTarget trait，处理 I/O 请求

⑤ /dev/ublkbN（内核块设备）
   OverlaybdTarget → UVMUblkCtrlBuilder::add_dev → set_params → start_dev
   内核创建 block device，queue worker 线程在 daemon 进程内轮询
   详见 overlaybd-commit-format.md §5

⑥ Firecracker VM（运行时）
   rootfs:  /dev/ublkbN 挂为 /dev/vda
   内存:    /dev/ublkbN mmap MAP_PRIVATE 作为 BackendType::File 内存后端
   同 template 的多 VM 共享同一 ublk 设备 → 块设备 page cache 复用 → 旁路 daemon
```

## 关键设计要点

- **commit 文件不可变**：sealed 后只读，允许多个 ImageFile 实例共享同一文件（靠 Linux page cache 去重）。
- **index 在内存合并，数据不合并**：多层 commit 各自独立打开，只把 segment mapping 数组合并成一个排序索引（新层覆盖旧层重叠区间，tag 标识层归属）。运行时是 stack 形态，不是扁平化文件。
- **内容寻址**：commit 文件按 sha256 digest 命名和寻址（`{repo}/managed-layers/{digest}.overlaybd.commit`），相同 digest 的层只存一份，跨 template/sandbox 去重。
- **ZFile 透明解压**：内存快照层通常 zstd 压缩，SwitchFile 自动检测 magic 并包一层 ZFileRO（随机访问跳表），对上层 LSMT 不可见。
- **ublk 而非 nbd/loop**：用 Linux ublk 驱动（6.8+），I/O 在 daemon 进程内用 io_uring 处理，零拷贝（AutoRegBuffer）+ 热池复用。
- **同 template 共享内存 ublk 设备**：`UblkDeviceManager` 按 image_config 路径做引用计数，多个 VM 复用同一个 `/dev/ublkbN` → page cache 命中后 daemon 被旁路。
- **跨 template 只有文件级 page cache 复用**：不同 template 的 mem_image.json 路径不同 → 不同 ublk 设备 → 块设备 page cache 各算各的，但公共层 commit 文件按 digest 内容寻址到同一 inode → 文件级 page cache 隐式共享（仅 buffered I/O 下有效）。

---

*后续文档按需补充各阶段细节。*

# image.json → ImageFile 打开流程

本文梳理从一个 `image.json` 配置文件到构建出运行时 `ImageFile` 对象的完整流程。
代码路径：`storage/overlaybd/src/image/image_file.rs` + `storage/overlaybd/src/lsmt/file/stack.rs`。

---

## 1. image.json 配置结构

`ImageConfig`（`storage/overlaybd/src/config.rs:146-157`）：

```json
{
  "repoBlobUrl": "",
  "lowers": [
    {
      "file": "/repo/managed-layers/sha256_abc.overlaybd.commit",
      "digest": "sha256:abc",
      "size": 1048576,
      "uuid": "00000000-0000-0000-0000-000000000abc"
    },
    {
      "file": "/repo/managed-layers/sha256_def.overlaybd.commit",
      "digest": "sha256:def",
      "size": 524288,
      "uuid": "00000000-0000-0000-0000-000000000def"
    }
  ],
  "upper": { "data": "", "index": "" },
  "resultFile": "./result.txt"
}
```

### 字段含义

| 字段 | 说明 |
|------|------|
| `lowers` | 只读层列表，**bottom-to-top** 顺序（`lowers[0]` = 最底层 base，`lowers[N-1` = 最新层） |
| `lowers[].file` | 本地 commit 文件绝对路径 |
| `lowers[].dir` | 目录形式（从中找 `commit` 或 `sealed` 子文件，与 `file` 二选一） |
| `lowers[].digest` | sha256 内容指纹（内容寻址、去重 key） |
| `lowers[].uuid` | overlaybd 层 UUID（链式 parent 校验用） |
| `lowers[].repo_blob_url` | 远程 OCI registry blob URL（本地缺失时回退下载） |
| `upper` | 可写层配置（`data` + `index` 文件路径），只读映像为空 |
| `upper.mode` | `sparse` / `logStructured`（默认）/ `hybridLogStructured` |

三种组合：
- `lowers` 非空 + `upper` 空 → 纯只读映像（内存快照 resume 用）
- `lowers` 空 + `upper` 非空 → 纯可写映像（新建空 rootfs）
- `lowers` 非空 + `upper` 非空 → 可写 stack（运行中的 rootfs）

---

## 2. 打开总流程

入口：`ImageFile` 的构造（`image_file.rs` 中通过 `ImageService::create_image_file` 调用）。

```
image.json
  │
  ▼
ImageFile::open(config, image_service, ...)
  │
  ├── open_lowers(config.lowers)          ← 并发打开所有只读层
  │     │
  │     │  对每个 lower:
  │     ├── open_localfile_path(layer)     解析 file/dir → 本地路径
  │     ├── open_ro_file(path)            LocalFile → TarFileAdaptor → SwitchFile
  │     │      (SwitchFile 自动检测 ZFile magic，是则包 ZFileRO)
  │     ├── 或 open_ro_p2p_uuid()         本地缺失时从 P2P 网络获取
  │     └── 或 open_ro_remote()           有 repo_blob_url 时从 registry 下载
  │     │
  │     │  得到 Vec<Arc<dyn VirtualFile>>>  （每层一个）
  │     │
  │     └── open_files_ro_with_premerged_cache(files, cache_dir, policy)
  │           从 N 个独立 commit 文件构建一个多层只读 stack：
  │           加载每层 index → 合并成一个无重叠排序数组 → 构建 LSMTReadOnlyFile
  │           （详见 §4）
  │
  ├── open_upper(config.upper)             ← 打开可写层（如果有）
  │     LocalFile::open_rw(upper.data) + LocalFile::open_rw(upper.index)
  │     → LSMTFile::open(data_file, index_file)
  │
  └── 组装 ImageFile
        │
        ├── (lower=Some, upper=Some) → stack_files(upper, lower)
        │     LSMTFile::open(upper_data, upper_index, Some(lower_index), lower.layers)
        │     → ImageFileBase::ReadWrite(stacked LSMTFile)
        │
        ├── (lower=Some, upper=None) → ImageFileBase::ReadOnly(LSMTReadOnlyFile)
        │
        └── (lower=None, upper=Some) → ImageFileBase::ReadWrite(LSMTFile)

  → ImageFile { state: LiveImageState { base, config, ... } }
```

---

## 3. 单层打开细节：open_ro_file

`image_file.rs:587-604`，每个只读 commit 文件经过三层适配器，每层都是"检查 magic → 是则包装 / 否则透传"：

```rust
async fn open_ro_file(path, image_service) -> Result<Arc<dyn VirtualFile>> {
    // 第一层：LocalFile —— 真正打开磁盘文件
    let file = LocalFile::builder(io_ring)
        .write(false).create(false).direct_io(direct_io)
        .open(path).await?;

    // 第二层：TarFileAdaptor —— 检查是否 tar 包裹
    let tar_file = new_tar_file_adaptor(file).await?;

    // 第三层：SwitchFile —— 检查是否 ZFile 压缩
    let switch = new_switch_file(tar_file, true, Some(path)).await?;
    Ok(switch)
}
```

### 3.1 第一层：LocalFile — 真正的文件句柄

`LocalFile`（`backend/local.rs:116-125`）是磁盘文件的直接包装：

```rust
pub struct LocalFile {
    path: PathBuf,
    file: Mutex<File>,        // tokio::fs::File
    direct_io: bool,
    io_ring: IoRingHandle,   // io_uring 实例
}
```

打开过程（`LocalFileBuilder::open`，`local.rs:91-113`）：
- `OpenOptions::new().read(true).write(false).create(false).open(path)` 拿到 fd。
- `direct_io` 由 `image_service.io_engine() == IO_ENGINE_LIBAIO`（值=2）决定。默认 `io_engine=0` → `direct_io=false` → 走 **buffered I/O**（读进 Linux page cache）。
- 如果 `direct_io=true`，追加 `O_DIRECT` flag。

读请求的分发（`read_at_via`，`local.rs:376-387`）：

```rust
async fn read_at_via(&self, submitter, offset, len) -> Result<Bytes> {
    if self.direct_io {
        return self.read_direct_at(submitter, offset, len).await;  // O_DIRECT 路径
    }
    self.read_buffered_at(submitter, offset, len).await             // buffered 路径
}
```

**buffered 路径**（`read_buffered_at`，`local.rs:294-303`）：
```rust
let fd = self.raw_fd().await;           // lock file mutex → as_raw_fd()
let mut buf = vec![0u8; len];
let n = io_ring::read_exact_at(submitter, fd, &mut buf, offset).await?;  // io_uring pread
```
→ 内核走 page cache：命中则不读盘，未命中则读盘并填入 page cache。

**O_DIRECT 路径**（`read_direct_at`，`local.rs:305-338`）：
```rust
// offset 和 len 可能不对齐 512B，需要扩展到对齐边界
let aligned_offset = align_down(offset, 512);
let aligned_end = align_up(offset + expected, 512);
let aligned_len = aligned_end - aligned_offset;
let mut buffer = AlignedBuffer::new(aligned_len, 512)?;   // 512B 对齐的内存
let got = io_ring::read_exact_at(submitter, fd, buffer.as_mut(), aligned_offset).await?;
// 从对齐读结果中截取 [head, head+expected) 返回
let buffer = buffer.into_sub_range(head..tail)?;
```
→ 绕过 page cache，直接 DMA。需要 512B 对齐的 offset/len/buffer pointer。

**无论后面套了什么适配器，真正读磁盘字节的只有这一层。** 后面的层只做 offset 转换和解压。

### 3.2 第二层：TarFileAdaptor — 跳过 tar 头

`new_tar_file_adaptor`（`tar.rs:696-698`）→ `open_tar_file`（`tar.rs:680-686`）：

```rust
pub async fn open_tar_file(file: Arc<dyn VirtualFile>) -> Result<Arc<dyn VirtualFile>> {
    if is_tar_file(file.as_ref()).await? {   // 检查 magic
        Ok(new_tar_file(file).await?)        // 是 tar → 包一层 TarFile
    } else {
        Ok(file)                             // 不是 tar → 原样返回
    }
}
```

**检测方式**（`is_tar_file`，`tar.rs:659-666`）：
```rust
let raw = file.read_at(0, 512).await?;           // 读前 512 字节
let header = TarHeader::from_block(&raw)?;
Ok(header.magic_matches()   // magic == "ustar" 或 "xxtar"(空 tar)
   && header.version_matches()  // version == "00"
   && header.crc_ok())          // CRC 校验
```

**什么时候是 tar**：OCI 标准镜像层是 tar 归档——tar header（512B）+ 实际 blob 数据 + tar padding。overlaybd-native 的 commit 文件**不是 tar**，直接是 LSMT 格式。

`TarFile`（`tar.rs:50-77`）做的事很简单：读 tar header 解析出 payload 的起始偏移（`base_offset`，通常 512B 或 1536B 有 PAX header 时），然后**所有读请求的 offset 加上 base_offset** 再转发给底层：

```rust
async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
    let max_len = size.saturating_sub(offset).min(len as u64) as usize;
    self.file.read_at(offset + self.base_offset, max_len).await
}
```

对上层来说，offset=0 就是 tar payload 的第一个字节（跳过了 tar header），完全透明。

**对于本地 `.overlaybd.commit` 文件**：不是 tar → `is_tar_file` 返回 false → 原样返回 LocalFile，这层不介入。

### 3.3 第三层：SwitchFile — 自动检测并透明解压 ZFile

`new_switch_file`（`switch.rs:98-116`）→ `try_open_zfile`（`switch.rs:14-34`）：

```rust
async fn try_open_zfile(file, verify, file_path) -> Result<Arc<dyn VirtualFile>> {
    let detected = is_zfile(file.clone()).await?;   // 检查 magic
    if detected == 1 {
        return zfile_open_ro_vfile(file, verify).await;  // 是 ZFile → 包 ZFileRO
    }
    Ok(file)                                              // 不是 → 原样返回
}
```

**检测方式**（`is_zfile`，`zfile.rs:2395-2408`）：
```rust
let mut buf = [0u8; 512];
read_exact_at(file.as_ref(), &mut buf, 0).await?;    // 读前 512 字节
let ht = HeaderTrailer::decode_from_512(&buf)?;
if !ht.verify_magic() || !ht.is_header() {          // magic == "ZFile\0\x01\0"
    return Ok(0);                                     // 不是 ZFile
}
ensure!(ht.is_valid(&buf), "header digest invalid"); // CRC32C 校验
Ok(1)
```

**什么时候是 ZFile**：内存快照层通常被 zstd 压缩（`OverlaybdCompactOutput::from_memory_snapshot_config`，rootfs 层不压缩 `OverlaybdCompactOutput::Raw`）。压缩后整个 LSMT 文件被包在 ZFile 容器里（ZFile header + zstd 数据块 + 跳表 + ZFile trailer）。

`ZFileRO`（`zfile.rs:839`）打开时调用 `load_jump_table`（`zfile.rs:1255-1290`）：
1. 读 ZFile header（offset 0，512B）→ 验证 magic + CRC → 取 `index_offset`, `index_size`, `original_file_size`, 压缩算法（zstd level 3 或 lz4）。
2. 读 ZFile trailer（末尾 512B）→ 验证 magic + sealed。
3. 从 `index_offset` 读跳表（`index_size × 4` 字节），建立 **compressed chunk → uncompressed offset** 的映射。

读请求 `pread(offset, len)` 时的路径：
```
用跳表二分查找包含 offset 的压缩 chunk
  → 从底层 VirtualFile pread 该 chunk 的压缩字节
  → 只解压这一个 chunk（zstd 随机访问，不需要全量解压）
  → 截取 [offset, offset+len) 返回
```

对上层来说，`ZFileRO` 就是一个普通 `VirtualFile`，`pread(0, N)` 返回的是**解压后的 LSMT 文件字节**（Header + Data + Index + Trailer），完全感知不到压缩。

**SwitchFile 的额外能力——延迟切换**（`switch.rs:74-95`）：

`SwitchFile` 内部持有 `m_file`（远程/压缩源）和 `m_local_file`（本地文件）。初始打开时可能用远程源（registryfs / P2P），后台下载完本地 commit 文件后调 `set_switch_file` 热切换到本地 `LocalFile`，后续 I/O 自动走本地路径。这是 overlaybd 按需下载的核心机制。

### 3.4 三种实际结果

同一个 `open_ro_file(path)` 调用，根据文件 magic 不同，返回的对象可能是以下三种之一：

**① 裸 LSMT commit（rootfs 层常用）**
```
文件头: "LSMT\0\1\2" (不是 tar, 不是 ZFile)
结果: LocalFile(path)
读路径: pread → io_uring → 内核 page cache → 磁盘
```

**② ZFile 压缩的 LSMT commit（内存快照层常用）**
```
文件头: "ZFile\0\x01\0" (不是 tar, 是 ZFile)
结果: ZFileRO(LocalFile(path))
读路径: pread(offset) → 跳表定位 chunk → 读 LocalFile 拿压缩字节 → 解压 → 返回解压后字节
```

**③ tar 包裹的 OCI 层（远程 OCI 镜像下载后）**
```
文件头: "ustar" (是 tar), 内部可能是裸 LSMT 或 ZFile
结果: TarFile(LocalFile(path))              — 内部是裸 LSMT
  或: ZFileRO(TarFile(LocalFile(path)))     — 内部是 ZFile
读路径: pread(offset) → offset + base_offset → pread → (如有 ZFile: 解压) → 磁盘
```

### 3.5 为什么是三层而不是一层

每层解决一个正交的问题：

| 层 | 解决的问题 | 什么时候介入 | 检测 magic |
|----|-----------|-------------|-----------|
| LocalFile | 磁盘 I/O（io_uring pread/pwrite、O_DIRECT 对齐） | 永远 | 无（直接打开文件） |
| TarFile | OCI tar 归档的 header 偏移 | 文件以 `"ustar"` 开头时 | 读前 512B 检查 tar header |
| ZFileRO | zstd 压缩的随机访问解压 | 文件以 `"ZFile\0\x01\0"` 开头时 | 读前 512B 检查 ZFile header |

三层各管各的，互不干扰。最终 LSMT 层（`LSMTReadOnlyFile`）拿到的 `VirtualFile` 无论经过了哪些层，`pread(offset, len)` 都返回**解压后的、跳过 tar 头的、原始 LSMT 字节**——所以 LSMT 不需要知道文件是否被 tar 包裹或 zstd 压缩。

### 3.6 统一返回类型：VirtualFile

`open_ro_file` 的返回类型是 `Arc<dyn VirtualFile>`（`image_file.rs:590`）。无论文件是裸 LSMT、ZFile 压缩、还是 tar 包裹，最终都装进同一个 trait object。

`VirtualFile`（`io/virtual_file.rs:42-165`）是一个"虚拟文件"抽象——暴露按字节偏移的读写接口，但不关心数据实际存在哪（磁盘文件、内存、压缩块、远程 registry 都行）。语义和 `pread`/`pwrite`/`fstat` 一一对应：

```rust
#[async_trait]
pub trait VirtualFile: Send + Sync {
    // 核心读写（必须实现）
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes>;
    async fn write_at(&self, offset: u64, data: &[u8]) -> Result<usize>;
    async fn size(&self) -> Result<u64>;

    // read_at 的零拷贝变体：直接写入调用方 buffer，避免中间分配
    async fn read_at_into(&self, offset: u64, dst: &mut [u8]) -> Result<usize>;

    // io_uring context 变体：ublk queue worker 线程用这个，携带线程局部 io_uring
    // 返回 !Send future（LocalSet 单线程模型），比 async fn 快
    fn read_at_with_ctx(&self, ctx: IoCtx, offset: u64, len: usize) -> LocalBoxFuture<Result<Bytes>>;
    fn read_at_into_with_ctx(&self, ctx: IoCtx, offset: u64, dst: &mut [u8]) -> LocalBoxFuture<Result<usize>>;
    fn write_at_with_ctx(&self, ctx: IoCtx, offset: u64, data: &[u8]) -> LocalBoxFuture<Result<usize>>;

    // 可选操作（有默认实现，不支持就 bail）
    async fn sync(&self) -> Result<()>;           // fsync
    async fn discard(&self, offset: u64, len: u64) -> Result<()>;  // hole punch（TRIM）
    async fn evict_range(&self, offset: u64, len: u64) -> Result<()>;  // 丢弃 page cache
    async fn evict_all(&self) -> Result<()>;
    async fn seek_data(&self, offset: u64) -> Result<Option<u64>>;
    async fn seek_hole(&self, offset: u64) -> Result<Option<u64>>;
    fn as_any(&self) -> Option<&dyn Any>;  // 类型擦除后取回具体类型（fast path 用）
}
```

实现这个 trait 的类型：

| 实现者 | 数据来源 | 在 open_ro_file 中的角色 |
|--------|---------|------------------------|
| `LocalFile` | 本地磁盘文件（io_uring pread） | 最内层，真正读盘 |
| `TarFile` | 内部 VirtualFile + tar header 偏移修正 | 中间层（OCI 层时介入） |
| `ZFileRO` | 内部 VirtualFile + zstd 解压 + 跳表 | 中间层（压缩层时介入） |
| `SwitchFile` | 内部 VirtualFile（可热切换 remote→local） | 最外层包装 |
| `RegistryFile` | OCI registry HTTP range 请求 | 远程层打开时用（不在 open_ro_file 里，在 open_ro_remote 里） |

调用方（`LSMTReadOnlyFile`、`ImageFile`）只调 `read_at(offset, len)` / `read_at_into_with_ctx(ctx, offset, dst)`，不关心底下套了几层。每层适配器内部把 offset 转换后转发给下一层，最终 `LocalFile` 执行真正的 `pread` 系统调用。

---

## 4. open_files_ro_with_premerged_cache：从多个文件构建多层只读 stack

`lsmt/file/stack.rs:156-220`，这是 `open_lowers` 的核心步骤。输入是 `open_ro_file` 打开好的各层 `VirtualFile`（bottom-to-top 顺序），输出是一个 `LSMTReadOnlyFile`（持有所有层文件 + 一个合并索引）。

### 4.0 整体流程

```
输入: Vec<Arc<dyn VirtualFile>>  (各层 commit 文件，bottom-to-top)
  │
  ├─ ① load_readonly_layers_metadata(files)
  │    每层读 Header(offset 0, 4096B) + Trailer(file_size - 4096, 4096B)
  │    → 验证 magic "LSMT\0\1\2" + sealed
  │    → 取出 index_offset, index_size, virtual_size, uuid, version
  │    → Vec<ReadOnlyLayerMetadata>
  │
  ├─ ② 算 cache key（如果配置了 premerged cache）
  │    PremergedIndexCacheKey::from_metadata(&metadata)
  │      = sha256( 所有层的 uuid + file_size + virtual_size
  │                + index_offset + index_size + header/trailer version )
  │    任意一层 uuid 为 nil → key = None → 跳过缓存，直接走 ④
  │
  ├─ ③ 尝试读 premerged index 缓存
  │    try_read_premerged_index_artifact(cache_dir, key)
  │      读 cache_dir/premerged-index/{digest_hex}.pmidx
  │      验证 magic "PMIDX001" + sha256 body 校验
  │    命中 → 直接反序列化为 merged ReadOnlyIndex → 跳到 ⑤
  │    未命中 → 走 ④
  │
  ├─ ④ merge_readonly_indexes(files, metadata)         ← cache miss 时走这
  │    │
  │    │  并发（buffer_unordered, 最多 PARALLEL_LOAD_INDEX=32 并发）
  │    │  对每层:
  │    │    load_index_and_reset_tags(file, index_offset, index_size)
  │    │      → 从 file 的 index_offset 处 pread index_size 字节
  │    │      → 按 16B stride 逐条 DiskSegmentMapping → to_memory() → SegmentMapping
  │    │      → 跳过 length==0 或 offset==u64::MAX 的无效条目
  │    │      → reset tag = 0
  │    │      → ReadOnlyIndex::new(mappings)
  │    │
  │    └─ ReadOnlyIndex::merge(&[各层 ReadOnlyIndex])
  │         按 top-to-bottom 顺序（i 从大到小）
  │         每层 mapping 赋 tag = i（tag 0 = 最新层）
  │         MutableIndex::insert 逐条插入：
  │           新 mapping [offset, offset+length) 覆盖旧重叠区间
  │           旧 mapping 被截断：重叠部分删掉，不重叠的左右保留
  │         → dump() 为无重叠、按 offset 排序的 Vec<SegmentMapping>
  │         → ReadOnlyIndex { mappings: sorted_vec }
  │
  │    （完成后异步写 .pmidx 缓存，供下次同 layer 组合命中）
  │
  └─ ⑤ build_readonly_stack_from_merged(files, metadata, merged)
       取 virtual_size（最后一个非零值，各层应一致）
       layers.reverse()  (bottom-to-top → top-to-bottom，匹配 merge 的 tag 顺序)
       uuids.reverse()
       → LSMTReadOnlyFile {
           layers: Vec<Arc<dyn VirtualFile>>,    // top-to-bottom
           index: Arc<ReadOnlyIndex>,             // 合并后的唯一索引
           virtual_size,
           uuids,
         }

输出: LSMTReadOnlyFile
```

### 4.1 为什么需要 merge

每层 commit 文件有自己独立的 `DiskSegmentMapping` 数组，描述"虚拟 offset → 本文件物理 offset"的映射。多层 stack 时，同一虚拟 offset 可能被多层覆盖——必须取**最新层**的数据。

如果不 merge，每次读都要**逐层查 index**（先查最新层，miss 再查下一层……），层数多时延迟线性增长。

merge 后只需**查一次合并索引**：返回的 `SegmentMapping` 带 `tag` 字段直接指向 `layers[tag]`，一步定位到正确的层和物理偏移。这是用一次性的 merge 开销换取每次读 O(1) 的 index 查找。

### 4.2 merge 的区间操作

`MutableIndex::insert`（`lsmt/index.rs`）的核心语义——基于排序数组的区间插入：

```
已有 mappings (按 offset 排序):
  [10, 20) → layer A
  [30, 50) → layer A

插入新 mapping [15, 40) → layer B (更新):
  1. 找到与 [15,40) 重叠的旧 mapping:
     [10,20) 重叠 [15,20) → 截断为 [10,15) 保留
     [30,50) 重叠 [30,40) → 截断为 [40,50) 保留
  2. 插入 [15,40) → layer B

结果 (无重叠、按 offset 排序):
  [10, 15) → layer A
  [15, 40) → layer B
  [40, 50) → layer A
```

新层的数据"遮盖"旧层在相同区间的映射，旧层不重叠的部分保留。这样读 `[15,40)` 时直接命中 layer B 的物理偏移，不需要回退到旧层。

### 4.3 premerged index 缓存

merge 是 O(N) 级别的区间操作（N = 所有层 mapping 总数）。当层组合固定时（同一 template 的多次 resume），merge 结果完全相同——缓存它可以跳过重复计算。

- **cache key**：`PremergedIndexCacheKey::from_metadata`（`helper.rs:247-287`）按所有层的 UUID + file_size + virtual_size + index_offset + index_size + header/trailer version 算 sha256。层组合变（增删层、层文件内容变）就自动失效。
- **读缓存**：`try_read_premerged_index_artifact`（`helper.rs:556-572`）从 `premerged-index/{digest_hex}.pmidx` 反序列化（带 magic `PMIDX001` + format version + sha256 body 校验）。
- **写缓存**：`spawn_premerged_index_artifact_write`（`helper.rs:514-554`）merge 完成后异步写 `.pmidx` 文件，带 LRU 淘汰（`prune_premerged_index_dir`，超过 `max_dir_bytes` 时删最旧）。
- **并发控制**：`acquire_premerged_index_lock`（`helper.rs:294-306`）按 `digest_hex` 串行化，防止多个并发请求重复 merge 同一组合。锁用 `Weak<Mutex<()>>` + `HashMap`，无人持有时自动清理。

命中缓存时跳过 ④ 的完整 merge，直接走 ⑤ 构建 stack。

---

## 5. 最终对象形态

### 5.1 纯只读（内存快照 resume）

```rust
ImageFile {
    state: LiveImageState {
        base: ImageFileBase::ReadOnly(
            LSMTReadOnlyFile {
                // 合并后的唯一索引
                index: Arc<ReadOnlyIndex {
                    mappings: [
                        SegmentMapping { offset: 0,    length: 2048, moffset: 8,  tag: 1 }, // layers[1]
                        SegmentMapping { offset: 2048, length: 4096, moffset: 16, tag: 0 }, // layers[0]
                        ...
                    ]
                }},
                // 各层独立文件，top-to-bottom 顺序
                layers: [
                    Arc<SwitchFile<ZFileRO<TarAdaptor<LocalFile>>>>,  // tag=0, 最新
                    Arc<SwitchFile<TarAdaptor<LocalFile>>>,           // tag=1, 更老
                    ...
                ],
                virtual_size: 2147483648,  // 2GB guest 内存
                uuids: [Uuid::..., ...],
            }
        ),
        config: ImageConfig { ... },
        ...
    }
}
```

### 5.2 两个核心变量：index 与 layers

`LSMTReadOnlyFile` 持有两样东西——index 是路由表，layers 是数据源，缺一不可。

#### index：合并后的路由表

`Arc<ReadOnlyIndex>`，内部是 `Vec<SegmentMapping>`——一个按虚拟 offset 排序、无重叠的数组。每条记录：

```rust
struct SegmentMapping {
    segment: Segment {
        offset: u64,    // 虚拟扇区号 (512B 单位)
        length: u32,    // 连续扇区数
    },
    moffset: u64,       // 该区间在 layers[tag] 文件中的物理扇区号 (512B 单位)
    zeroed:  bool,      // true = 这段是零块，不需要读文件
    tag:     u8,        // layers[tag]，指向哪个层文件
}
```

查的时候二分定位（`index.rs:214-243`）：`partition_point` 找到第一个 `end > query.offset` 的条目，往后连续取直到超出 query 范围。一个读请求可能命中多条 mapping——因为读区间可能跨越多个归属不同层的区段或中间的 hole：

```
读请求: 虚拟 [0, 8) 扇区

merged index (部分):
  [0, 3)   → tag 1, moffset=20
  [3, 5)   → tag 0, moffset=8      ← 不同层
  [5, 6)   hole（没有 mapping）
  [6, 10)  → tag 1, moffset=23

lookup 返回 3 条 mapping（截断到 [0,8) 范围内）:
  [0, 3)  → tag 1  → pread(layers[1], 20×512, 3×512)
  [3, 5)  → tag 0  → pread(layers[0], 8×512, 2×512)
  [6, 8)  → tag 1  → pread(layers[1], 23×512, 2×512)
  [5, 6)  → hole   → buf 填零
```

一个 4096 字节的读请求被拆成三段 pread（去两个不同的文件）加一段填零，结果拼进同一个 buffer 返回（`readonly.rs:230-299` 的循环逻辑）。

index 本身不存数据，只告诉你"这个虚拟 offset 该去哪层、哪个物理偏移读"。

#### layers：各层 commit 的 VirtualFile 句柄

`Vec<Arc<dyn VirtualFile>>`，每项是一个 commit 文件经适配器包装后的 `VirtualFile` 对象（LocalFile → TarFile → ZFileRO → SwitchFile，按需套，详见 §3）。top-to-bottom 顺序，`layers[0]` = 最新层 = `tag=0`。

调用 `read_at(offset, len)` 就能读到该 commit 文件**解压后、跳过 tar 头的原始 LSMT 字节**——包括 Header、Data 区、Index 区、Trailer。`LSMTReadOnlyFile` 读数据页时直接对这个 `VirtualFile` 调 `read_at(moffset * 512, len)` 取物理偏移处的字节。

另外加载 index 时（`load_index_and_reset_tags`、`load_readonly_layers_metadata`）也是对同一个 `VirtualFile` 调 `read_at` 读 Header/Trailer/Index 区——整个 commit 文件的字节都可以通过这个接口访问，不区分元数据和数据。

#### 两者配合

读路径（`readonly.rs:271-281`）：

```rust
// ① 从 index 查到这条映射
let phys_offset = m.moffset * ALIGNMENT;
let layer_idx = m.tag as usize;
// ② 从 layers 里取出对应的文件，去它的物理偏移读
read_exact(reader, self.layers[layer_idx].as_ref(), phys_offset, &mut buf[..])
```

index 负责"去哪找"，layers 负责"真正读出字节"。

### 5.3 可写 stack（运行中 rootfs）

```rust
ImageFile {
    state: LiveImageState {
        base: ImageFileBase::ReadWrite(
            LSMTFile {
                rw_data_file: LocalFile("upper.data"),   // 可写，log-structured append
                rw_index_file: LocalFile("upper.index"), // 可写，追加 DiskSegmentMapping
                lower_index: Some(Arc<ReadOnlyIndex { ... }>),  // 合并的只读层索引
                lower_layers: vec![Arc<SwitchFile<...>>, ...],  // 各 commit 文件
                virtual_size: AtomicU64(1073741824),             // 1GB rootfs
                ...
            }
        ),
        ...
    }
}
```

读时 `LSMTFile` 先查 upper 的 in-memory index（新写的数据），miss 再查 `lower_index`（合并的只读层索引）。
写时 append 到 `upper.data` + `upper.index`，不碰任何只读层。

---

## 6. 关键不变量

1. **commit 文件永不修改**：sealed 后只读，`open_ro_file` 以 `.write(false)` 打开。多个 `ImageFile` 可安全共享同一文件。

2. **layers 和 index 的 tag 对应关系**：merged index 中 `tag=0` 对应 `layers[0]`（top-to-top 排序后的最新层）。`build_readonly_stack_from_merged` 在 merge 后做 `layers.reverse()` 把 bottom-to-top 转成 top-to-bottom。

3. **virtual_size 来自 trailer**：每层 trailer 的 `virtual_size` 字段记录虚拟磁盘大小，所有层应一致（同一 stack 的各层 virtual_size 相同）。`build_readonly_stack_from_merged` 取最后一个非零值。

4. **index merge 是 O(N) 级别的区间操作**：不是逐扇区扫描，而是基于 `Vec<SegmentMapping>` 的排序数组 + `MutableIndex::insert` 的区间截断/插入。层数 ≤ 255（`MAX_STACK_LAYERS`）。

5. **premerged cache 是可选优化**：不命中缓存时走完整 merge，命中时跳过。cache key 包含所有层的 UUID + size + index 位置，层组合变就失效。

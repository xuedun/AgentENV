//! Shared cgroup v2 shards for the Firecracker process pool.
//!
//! Each spawned Firecracker process moves into one of a fixed number of leaf
//! cgroups under `/sys/fs/cgroup/aenv`, so its anonymous memory and page
//! cache are charged to that shard's memcg: reclaim and per-memcg writeback
//! triggered by one sandbox (a resume decompressing its memory image back
//! into the page cache, for example) stay scoped to that shard instead of
//! churning the shared global lists that every co-located sandbox depends
//! on. With the fleet spread round-robin, one sandbox's pressure lands on
//! its shard mates instead of every sandbox in the root memcg.
//!
//! The shard count is fixed and created once at server startup
//! (`firecracker-slot-0..pool_size`); spawns only move pids in, and nothing
//! is ever removed at runtime — a dead process empties its slot in the memcg
//! by itself, and the leaf outlives every occupant. The count is a config
//! decision (`[firecracker] cgroup_pool_size`) because per-leaf kernel cost
//! scales with the host's CPU and NUMA-node counts: every memcg's per-CPU
//! statistics are charged to the parent cgroup, so on a 64-node host each
//! leaf holds tens of MB there, and a pool that grows with total spawns
//! makes the parent's `percpu` figure grow without bound. Runtime leaf
//! churn also does not survive load: mkdir, rmdir, and `cgroup.procs`
//! writes all serialize on the kernel's global `cgroup_mutex`, and every
//! rmdir queues memcg-offline work on a single-slot workqueue that runs
//! under that same mutex. Startup reconciliation removes leaves a previous
//! run left behind whose processes are gone, but keeps the shard slots.
//!
//! The server runs as the non-root runtime account, so `server --setup-host`
//! provisions the base and hands it to the runtime group; this module only
//! creates the shards in it at startup.
//!
//! An optional `[firecracker] cgroup_cpuset_topology` additionally binds
//! each shard to one hardware domain via cpuset when the pool is created:
//! `ccl` (the default) pins the shard's CPUs to one kernel cluster (the CCL
//! / L3 domain on Kunpeng) and its memory allocations to the containing
//! NUMA node, `numa` pins both to one node. Groups come from sysfs
//! (`cluster_cpus_list`, falling back to the L3 `shared_cpu_list`, plus
//! `/sys/devices/system/node`), sorted by their lowest CPU so the
//! slot-to-group mapping is stable across restarts; detection and the mask
//! writes happen once here, best-effort like the pool itself. CCL binding
//! applies on arm64 hosts with cluster topology; everything else leaves
//! the shards unbound.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use tracing::{debug, info, warn};

use crate::cfg::CpusetTopology;

/// Mount point of the cgroup v2 unified hierarchy.
const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";
/// Parent cgroup holding the leaf pool; provisioned by `server --setup-host`.
const BASE_DIR_NAME: &str = "aenv";
/// Leaf prefix shared by pooled (`firecracker-slot-<n>`) and legacy
/// (`firecracker-<pid>`) leaves; reconciliation only touches directories
/// carrying it.
const LEAF_PREFIX: &str = "firecracker-";
/// Prefix of the pooled slot names, whose index decides whether startup
/// reconciliation keeps the directory or reclaims it.
const SLOT_PREFIX: &str = "firecracker-slot-";

/// Set after the first isolation failure so a mis-provisioned host warns once
/// instead of once per spawned process.
static UNAVAILABLE_WARNED: AtomicBool = AtomicBool::new(false);
/// Set when the leaf pool was created, so `ensure_leaf_pool` runs once.
static POOL_READY: AtomicBool = AtomicBool::new(false);
/// Round-robin cursor over the shard leaves.
static NEXT_LEAF: AtomicU64 = AtomicU64::new(0);
/// The shard leaves, in creation order.
static LEAVES: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// The cgroup v2 unified hierarchy, or `None` on hosts without one.
pub(crate) fn cgroup_v2_root() -> Option<&'static Path> {
    let root = Path::new(CGROUP_V2_ROOT);
    root.join("cgroup.controllers").is_file().then_some(root)
}

/// The base cgroup under `root` that holds the leaf pool.
pub(crate) fn base_dir(root: &Path) -> PathBuf {
    root.join(BASE_DIR_NAME)
}

fn is_leaf_name(name: &str) -> bool {
    name.starts_with(LEAF_PREFIX)
}

/// The pooled slot index of a leaf name, or `None` for legacy leaves.
fn slot_index(name: &str) -> Option<usize> {
    name.strip_prefix(SLOT_PREFIX)?.parse().ok()
}

/// Removes leaves left behind by a previous run whose processes are gone:
/// the kernel empties a leaf when its process dies but never removes the
/// directory, and the pool only exists in the old process's memory. Shard
/// slots (`firecracker-slot-<n>` with `n < pool_size`) are kept either way —
/// [`ensure_leaf_pool`] reuses them in place, and rmdir'ing them here would
/// queue the single-slot memcg-offline work once per leaf right at startup.
/// Leaves that still hold a live process are left alone. Called once at
/// server startup, before any spawn can race it.
pub(crate) fn remove_unoccupied_leaves(pool_size: usize) {
    let Some(root) = cgroup_v2_root() else {
        return;
    };
    remove_unoccupied_leaves_in(&base_dir(root), pool_size);
}

/// The filesystem half of [`remove_unoccupied_leaves`], against any
/// directory laid out like the cgroup base.
fn remove_unoccupied_leaves_in(base: &Path, pool_size: usize) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let procs = fs::read_to_string(entry.path().join("cgroup.procs")).ok();
        if !is_removable_leaf(&name, procs.as_deref(), pool_size) {
            continue;
        }
        if let Err(err) = fs::remove_dir(entry.path()) {
            if err.kind() != ErrorKind::NotFound {
                warn!(
                    path = %entry.path().display(),
                    err = %err,
                    "failed to remove an unoccupied firecracker cgroup leaf"
                );
            }
        } else {
            removed += 1;
        }
    }
    if removed > 0 {
        info!(
            removed,
            base = %base.display(),
            "removed firecracker cgroup leaves left over from a previous run"
        );
    }
}

/// Whether startup reconciliation removes a leaf directory: it must carry
/// the leaf prefix, not be a pooled slot below `pool_size`, and hold no
/// live process. A missing or unreadable `cgroup.procs` counts as occupied.
fn is_removable_leaf(name: &str, procs: Option<&str>, pool_size: usize) -> bool {
    if !is_leaf_name(name) {
        return false;
    }
    if slot_index(name).is_some_and(|slot| slot < pool_size) {
        return false;
    }
    procs.is_some_and(|procs| procs.trim().is_empty())
}

/// Creates the fixed leaf pool once at server startup: every slot of
/// `0..pool_size` that a previous run left behind is reused in place (an
/// orphan still inside one just shares it, which is the design), the rest
/// are created. Called once; later calls are no-ops. With `pool_size = 0`
/// the isolation stays off and every spawn runs ungrouped. With a
/// `topology` other than [`CpusetTopology::None`], slot `n` is also bound
/// to topology group `n % groups` — stale masks a differently configured
/// previous run left on reused slots are rewritten or reset in place.
pub(crate) fn ensure_leaf_pool(pool_size: usize, topology: CpusetTopology) {
    if pool_size == 0 || POOL_READY.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(root) = cgroup_v2_root() else {
        return;
    };
    let base = base_dir(root);
    let bindings = topology_bindings(topology);
    if topology != CpusetTopology::None && bindings.is_none() {
        warn!(
            ?topology,
            "no usable CPU topology groups; firecracker cgroup shards stay unbound"
        );
    }
    let pooled = ensure_leaf_pool_in(&base, pool_size, topology, bindings.as_deref());
    *LEAVES.lock().unwrap() = pooled;
}

/// The filesystem half of [`ensure_leaf_pool`], against any directory laid
/// out like the cgroup base; returns the pooled slot paths in slot order.
fn ensure_leaf_pool_in(
    base: &Path,
    pool_size: usize,
    topology: CpusetTopology,
    bindings: Option<&[GroupBinding]>,
) -> Vec<PathBuf> {
    let mut created = 0;
    let mut reused = 0;
    let mut skipped = 0;
    let mut bind_failed = false;
    let mut leaves = Vec::new();
    for slot in 0..pool_size {
        let leaf = base.join(format!("{SLOT_PREFIX}{slot}"));
        if !leaf.is_dir() {
            if let Err(err) = fs::create_dir(&leaf) {
                skipped += 1;
                warn!(
                    path = %leaf.display(),
                    err = %err,
                    "failed to create a pooled firecracker cgroup leaf"
                );
                continue;
            }
            created += 1;
        } else {
            reused += 1;
        }
        let binding = bindings.map(|groups| &groups[slot % groups.len()]);
        if let Err(err) = apply_binding(&leaf, base, binding) {
            if !bind_failed {
                bind_failed = true;
                warn!(
                    leaf = %leaf.display(),
                    err = %err,
                    "failed to write a firecracker shard leaf's cpuset masks; \
                     shards stay unbound (is `cpuset` enabled down to the base?)"
                );
            }
        }
        leaves.push(leaf);
    }
    info!(
        pool_size,
        created,
        reused,
        skipped,
        topology = ?topology,
        base = %base.display(),
        "firecracker cgroup leaf pool is ready"
    );
    leaves
}

/// Moves the freshly spawned process into the next shard leaf,
/// round-robin.
///
/// Isolation stays best-effort like the adjacent `oom_score_adj` write:
/// without a provisioned base the process runs ungrouped and the reason is
/// logged once, not once per spawn.
pub(crate) fn attach(pid: u32) {
    if cgroup_v2_root().is_none() {
        debug!(
            pid,
            "no cgroup v2 hierarchy; firecracker runs without a per-instance cgroup"
        );
        return;
    }
    let leaf = {
        let leaves = LEAVES.lock().unwrap();
        if leaves.is_empty() {
            // Only a pool_size of 0 or a failed startup provisioning lands
            // here, and both already said so at startup.
            debug!(pid, "no cgroup shard leaves; firecracker runs ungrouped");
            return;
        }
        let index = NEXT_LEAF.fetch_add(1, Ordering::Relaxed) as usize % leaves.len();
        leaves[index].clone()
    };
    // posix_spawn has returned, so exec has been initiated: the move can
    // still race firecracker's earliest startup, but its large allocations
    // (guest memory) only happen later, on the snapshot load path, and the
    // whole thread group moves either way.
    if let Err(err) = fs::write(leaf.join("cgroup.procs"), pid.to_string()) {
        let message = format!(
            "failed to move firecracker into its cgroup shard leaf: {err:#}; \
             run `server --setup-host` as root to provision it"
        );
        if !UNAVAILABLE_WARNED.swap(true, Ordering::Relaxed) {
            warn!(pid, leaf = %leaf.display(), "{message}");
        } else {
            debug!(pid, leaf = %leaf.display(), "{message}");
        }
    }
}

/// The cpuset masks written into one pool shard leaf: kernel-format
/// `cpuset.cpus`, plus `cpuset.mems` when a containing node was found.
struct GroupBinding {
    cpus: String,
    mems: Option<String>,
}

/// The shard groups for `topology`, or `None` when the topology is `none`
/// or nothing could be detected; callers warn once in the second case. CCL
/// binding applies on arm64 only — the kernel cluster topology it is shaped
/// for (the CCL on Kunpeng) is an arm64 concept, so other architectures
/// leave the shards unbound.
fn topology_bindings(topology: CpusetTopology) -> Option<Vec<GroupBinding>> {
    match topology {
        CpusetTopology::None => None,
        CpusetTopology::Numa => {
            let groups = bind_nodes(&numa_nodes());
            (!groups.is_empty()).then_some(groups)
        }
        CpusetTopology::Ccl if cfg!(target_arch = "aarch64") => {
            let groups = bind_clusters(&cluster_groups(), &numa_nodes());
            (!groups.is_empty()).then_some(groups)
        }
        CpusetTopology::Ccl => None,
    }
}

/// Writes one leaf's cpuset masks. With a binding this restricts the leaf;
/// without one it resets a leaf a previous run may have bound back to the
/// inherited defaults, so switching the topology off really releases it.
/// Missing files mean the cpuset controller is not enabled down to the leaf
/// and nothing can be written either way; that is not an error.
fn apply_binding(leaf: &Path, base: &Path, binding: Option<&GroupBinding>) -> std::io::Result<()> {
    let write = |file: &str, value: &str| -> std::io::Result<()> {
        match fs::write(leaf.join(file), value) {
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    match binding {
        Some(group) => {
            write("cpuset.cpus", &group.cpus)?;
            if let Some(mems) = &group.mems {
                write("cpuset.mems", mems)?;
            }
            Ok(())
        }
        None => {
            // The leaf defaults to its parent's effective range, so writing
            // it back is the unbound state.
            let reset = |file: &str| fs::read_to_string(base.join(file)).unwrap_or_default();
            write("cpuset.cpus", &reset("cpuset.cpus.effective"))?;
            write("cpuset.mems", &reset("cpuset.mems.effective"))
        }
    }
}

/// One `numa` group per node: `cpus` and `mems` both cover the node.
fn bind_nodes(node_list: &[(String, BTreeSet<u32>)]) -> Vec<GroupBinding> {
    node_list
        .iter()
        .map(|(id, cpus)| GroupBinding {
            cpus: format_cpu_list(cpus),
            mems: Some(id.clone()),
        })
        .collect()
}

/// One `ccl` group per cluster: `cpus` covers the cluster's logical CPUs,
/// `mems` the NUMA node containing it. Clusters never straddle nodes on
/// real hardware; one that does simply gets no `mems` write and keeps the
/// inherited node set.
fn bind_clusters(
    clusters: &[BTreeSet<u32>],
    node_list: &[(String, BTreeSet<u32>)],
) -> Vec<GroupBinding> {
    clusters
        .iter()
        .map(|cpus| GroupBinding {
            cpus: format_cpu_list(cpus),
            mems: node_list
                .iter()
                .find(|(_, node_cpus)| node_cpus.is_superset(cpus))
                .map(|(id, _)| id.clone()),
        })
        .collect()
}

/// Distinct kernel cluster CPU sets, one per CCL on Kunpeng-style hardware,
/// sorted by lowest CPU. Falls back to the L3 cache domains when the kernel
/// does not expose usable cluster topology; at most one distinct group
/// means there is no CCL to bind and yields an empty list.
fn cluster_groups() -> Vec<BTreeSet<u32>> {
    let mut groups = unique_cpu_groups("topology/cluster_cpus_list");
    if groups.len() <= 1 {
        let l3 = unique_cpu_groups("cache/index3/shared_cpu_list");
        if l3.len() > groups.len() {
            groups = l3;
        }
    }
    groups.sort_by_key(|g| g.first().copied().unwrap_or(0));
    groups
}

/// The host's NUMA nodes as `(node id, cpu set)`, ordered by id.
fn numa_nodes() -> Vec<(String, BTreeSet<u32>)> {
    let Ok(entries) = fs::read_dir("/sys/devices/system/node") else {
        return Vec::new();
    };
    let mut node_list: Vec<(u32, BTreeSet<u32>)> = Vec::new();
    for entry in entries.flatten() {
        let id = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("node"))
            .and_then(|rest| rest.parse::<u32>().ok());
        let Some(id) = id else { continue };
        let Some(cpus) = fs::read_to_string(entry.path().join("cpulist"))
            .ok()
            .and_then(|text| parse_cpu_list(&text))
            .filter(|set| !set.is_empty())
        else {
            continue;
        };
        node_list.push((id, cpus));
    }
    node_list.sort_by_key(|(id, _)| *id);
    node_list
        .into_iter()
        .map(|(id, cpus)| (id.to_string(), cpus))
        .collect()
}

/// Reads one CPU list file per `cpuN` directory under `/sys/devices/system/cpu`
/// and returns the distinct non-empty sets. Offline CPUs carry empty files
/// and are skipped.
fn unique_cpu_groups(relative: &str) -> Vec<BTreeSet<u32>> {
    let mut groups: Vec<BTreeSet<u32>> = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
        return groups;
    };
    for entry in entries.flatten() {
        let is_cpu = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("cpu"))
            .and_then(|rest| rest.parse::<u32>().ok())
            .is_some();
        if !is_cpu {
            continue;
        }
        let Some(set) = fs::read_to_string(entry.path().join(relative))
            .ok()
            .and_then(|text| parse_cpu_list(&text))
            .filter(|set| !set.is_empty())
        else {
            continue;
        };
        if !groups.contains(&set) {
            groups.push(set);
        }
    }
    groups
}

/// Parses a kernel CPU list (`"0-15,192-207"`) into a set; `None` on
/// malformed input. An empty string yields an empty set.
fn parse_cpu_list(text: &str) -> Option<BTreeSet<u32>> {
    let mut cpus = BTreeSet::new();
    for part in text.trim().split(',') {
        if part.is_empty() {
            continue;
        }
        let (start, end) = match part.split_once('-') {
            Some((start, end)) => (start.parse::<u32>().ok()?, end.parse::<u32>().ok()?),
            None => {
                let cpu = part.parse::<u32>().ok()?;
                (cpu, cpu)
            }
        };
        if end < start {
            return None;
        }
        cpus.extend(start..=end);
    }
    Some(cpus)
}

/// Formats a CPU set back into kernel list form (`"0-15,192-207"`).
fn format_cpu_list(cpus: &BTreeSet<u32>) -> String {
    let mut out = String::new();
    let mut run: Option<(u32, u32)> = None;
    for &cpu in cpus {
        match &mut run {
            Some((_, end)) if *end + 1 == cpu => *end = cpu,
            _ => {
                push_run(&mut out, run);
                run = Some((cpu, cpu));
            }
        }
    }
    push_run(&mut out, run);
    out
}

fn push_run(out: &mut String, run: Option<(u32, u32)>) {
    if let Some((start, end)) = run {
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            let _ = write!(out, "{start}");
        } else {
            let _ = write!(out, "{start}-{end}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_name_matches_current_and_legacy_leaves() {
        assert!(is_leaf_name("firecracker-slot-0"));
        assert!(is_leaf_name("firecracker-slot-42"));
        assert!(is_leaf_name("firecracker-1234"));
        assert!(!is_leaf_name("firecracker"));
        assert!(!is_leaf_name("init-1"));
    }

    #[test]
    fn slot_index_parses_pooled_slot_names_only() {
        assert_eq!(slot_index("firecracker-slot-0"), Some(0));
        assert_eq!(slot_index("firecracker-slot-127"), Some(127));
        assert_eq!(slot_index("firecracker-1234"), None);
        assert_eq!(slot_index("firecracker-slot-x"), None);
    }

    #[test]
    fn parses_kernel_cpu_lists() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11\n"),
            Some(BTreeSet::from([0, 1, 2, 3, 8, 10, 11]))
        );
        assert_eq!(parse_cpu_list(""), Some(BTreeSet::new()));
        assert_eq!(parse_cpu_list("5"), Some(BTreeSet::from([5])));
        assert_eq!(parse_cpu_list("8-3"), None);
        assert_eq!(parse_cpu_list("x"), None);
        assert_eq!(parse_cpu_list("1-,2"), None);
    }

    #[test]
    fn formats_sorted_runs() {
        let cpus = BTreeSet::from([0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(format_cpu_list(&cpus), "0-3,8,10-11");
        assert_eq!(format_cpu_list(&BTreeSet::new()), "");
    }

    #[test]
    fn maps_clusters_to_containing_nodes() {
        // The Kunpeng 950 shape: 24 clusters of 16 logical CPUs across
        // 4 half-socket nodes of 6 clusters each.
        let clusters: Vec<BTreeSet<u32>> = (0..24)
            .map(|c| ((c * 16)..(c * 16 + 16)).collect())
            .collect();
        let node_list: Vec<(String, BTreeSet<u32>)> = (0..4)
            .map(|n| (n.to_string(), ((n * 96)..(n * 96 + 96)).collect()))
            .collect();
        let groups = bind_clusters(&clusters, &node_list);
        assert_eq!(groups.len(), 24);
        assert_eq!(groups[0].cpus, "0-15");
        assert_eq!(groups[0].mems.as_deref(), Some("0"));
        assert_eq!(groups[6].cpus, "96-111");
        assert_eq!(groups[6].mems.as_deref(), Some("1"));
        assert_eq!(groups[23].cpus, "368-383");
        assert_eq!(groups[23].mems.as_deref(), Some("3"));
    }

    #[test]
    fn clusters_without_a_node_lose_only_their_mems() {
        let clusters = vec![BTreeSet::from([0, 1]), BTreeSet::from([2, 3])];
        let node_list = vec![("0".to_string(), BTreeSet::from([0, 1]))];
        let groups = bind_clusters(&clusters, &node_list);
        assert_eq!(groups[0].cpus, "0-1");
        assert_eq!(groups[0].mems.as_deref(), Some("0"));
        assert_eq!(groups[1].cpus, "2-3");
        assert_eq!(groups[1].mems, None);
    }

    #[test]
    fn binds_nodes_to_their_own_cpus_and_ids() {
        let node_list = vec![
            ("0".to_string(), BTreeSet::from([0, 1, 2, 3])),
            ("1".to_string(), BTreeSet::from([4, 5, 6, 7])),
        ];
        let groups = bind_nodes(&node_list);
        assert_eq!(groups[0].cpus, "0-3");
        assert_eq!(groups[0].mems.as_deref(), Some("0"));
        assert_eq!(groups[1].cpus, "4-7");
        assert_eq!(groups[1].mems.as_deref(), Some("1"));
    }

    #[test]
    fn reconciliation_keeps_slots_and_occupied_leaves() {
        // A tempdir cannot fake a rmdir-able cgroup directory (its
        // cgroup.procs would be a real file), so the removal decisions are
        // covered by `is_removable_leaf` below; this exercises the scan.
        let base = tempfile::tempdir().unwrap();
        let leaf = |name: &str, procs: Option<&str>| {
            let dir = base.path().join(name);
            fs::create_dir(&dir).unwrap();
            if let Some(procs) = procs {
                fs::write(dir.join("cgroup.procs"), procs).unwrap();
            }
            dir
        };
        let kept = [
            leaf("firecracker-slot-0", Some("")),
            leaf("firecracker-slot-5", Some("42\n")),
            leaf("firecracker-slot-200", Some("42\n")),
            leaf("firecracker-1000", Some("7\n")),
            // No cgroup.procs file reads as occupied: kept conservatively.
            leaf("firecracker-1001", None),
            leaf("init-1", Some("")),
        ];

        remove_unoccupied_leaves_in(base.path(), 128);

        for dir in kept {
            assert!(dir.is_dir(), "{} should be kept", dir.display());
        }
    }

    #[test]
    fn leaf_disposition_decides_removal() {
        // Pooled slots below pool_size are always kept.
        assert!(!is_removable_leaf("firecracker-slot-0", Some(""), 128));
        // A slot beyond pool_size with no live process is removed.
        assert!(is_removable_leaf("firecracker-slot-200", Some(""), 128));
        assert!(is_removable_leaf("firecracker-slot-200", Some("  \n"), 128));
        // A live process keeps even a stale slot.
        assert!(!is_removable_leaf(
            "firecracker-slot-200",
            Some("42\n"),
            128
        ));
        // Legacy leaves follow the same process rule.
        assert!(is_removable_leaf("firecracker-999", Some(""), 128));
        assert!(!is_removable_leaf("firecracker-999", Some("42\n"), 128));
        // Missing or unreadable cgroup.procs counts as occupied.
        assert!(!is_removable_leaf("firecracker-999", None, 128));
        // Names outside the leaf namespace are never touched.
        assert!(!is_removable_leaf("init-1", Some(""), 128));
        assert!(!is_removable_leaf("firecracker", Some(""), 128));
    }

    #[test]
    fn pool_creation_reuses_stale_slots_and_writes_bindings() {
        let base = tempfile::tempdir().unwrap();
        // A previous run left slot 0 behind, bound to a different domain.
        fs::create_dir(base.path().join("firecracker-slot-0")).unwrap();
        fs::write(base.path().join("firecracker-slot-0/cpuset.cpus"), "99").unwrap();
        let bindings = vec![
            GroupBinding {
                cpus: "0-3".into(),
                mems: Some("0".into()),
            },
            GroupBinding {
                cpus: "4-7".into(),
                mems: None,
            },
        ];

        let leaves = ensure_leaf_pool_in(base.path(), 3, CpusetTopology::None, Some(&bindings));

        assert_eq!(leaves.len(), 3);
        let slot = |n: usize| base.path().join(format!("firecracker-slot-{n}"));
        assert!(slot(0).is_dir() && slot(1).is_dir() && slot(2).is_dir());
        // Slot 0's stale mask is rewritten in place; slot 2 wraps around.
        assert_eq!(
            fs::read_to_string(slot(0).join("cpuset.cpus")).unwrap(),
            "0-3"
        );
        assert_eq!(
            fs::read_to_string(slot(1).join("cpuset.cpus")).unwrap(),
            "4-7"
        );
        assert_eq!(
            fs::read_to_string(slot(2).join("cpuset.cpus")).unwrap(),
            "0-3"
        );
        assert_eq!(
            fs::read_to_string(slot(0).join("cpuset.mems")).unwrap(),
            "0"
        );
        assert!(!slot(1).join("cpuset.mems").exists());
    }

    #[test]
    fn unbound_pool_resets_stale_masks_to_the_inherited_range() {
        let base = tempfile::tempdir().unwrap();
        fs::write(base.path().join("cpuset.cpus.effective"), "0-15\n").unwrap();
        fs::write(base.path().join("cpuset.mems.effective"), "0\n").unwrap();
        fs::create_dir(base.path().join("firecracker-slot-0")).unwrap();
        fs::write(base.path().join("firecracker-slot-0/cpuset.cpus"), "99").unwrap();

        let leaves = ensure_leaf_pool_in(base.path(), 1, CpusetTopology::None, None);

        assert_eq!(leaves.len(), 1);
        let slot0 = base.path().join("firecracker-slot-0");
        assert_eq!(
            fs::read_to_string(slot0.join("cpuset.cpus")).unwrap(),
            "0-15\n"
        );
        assert_eq!(
            fs::read_to_string(slot0.join("cpuset.mems")).unwrap(),
            "0\n"
        );
    }
}

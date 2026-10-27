//! Host provisioning for the per-Firecracker cgroup base.
//!
//! Creating the cgroup base and enabling the memory controller on it are
//! privileged operations. A root-privileged server does both by itself at
//! startup ([`ensure_runtime_base`]); a server running as the non-root
//! runtime account relies on `--setup-host` ([`provision`]) to do them and
//! hand the base directory to the runtime group, so the server only ever
//! creates and removes its own leaves in it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::unistd::{chown, Gid};
use tracing::{info, warn};

use crate::cfg::CpusetTopology;
use crate::sandbox::firecracker::cgroup::{
    base_dir, cgroup_v2_root, ensure_leaf_pool, remove_unoccupied_leaves,
};

/// Controllers enabled down to the base: `memory` charges each leaf's
/// Firecracker process, `cpuset` carries the optional topology masks.
const MEMORY_CONTROLLER: &str = "memory";
const CPUSET_CONTROLLER: &str = "cpuset";

/// Creates the per-Firecracker cgroup base with the memory and cpuset
/// controllers enabled and hands it to the runtime group. A no-op on hosts
/// without cgroup v2.
pub(super) fn provision(runtime_gid: Gid) -> Result<()> {
    let Some(root) = cgroup_v2_root() else {
        info!("no cgroup v2 hierarchy; per-firecracker memcg isolation is unavailable");
        return Ok(());
    };
    let base = prepare_base(root)?;
    // Leaves are created by the server itself and stay owned by it; group
    // write on the base is all the account needs.
    chown(&base, None, Some(runtime_gid))
        .with_context(|| format!("set runtime group ownership on {}", base.display()))?;
    fs::set_permissions(&base, fs::Permissions::from_mode(0o775))
        .with_context(|| format!("set runtime group access on {}", base.display()))?;
    Ok(())
}

/// Self-provisions the base during normal server startup, so privileged
/// servers get the isolation without running `--setup-host` first, then
/// creates the fixed leaf pool of `[firecracker] cgroup_pool_size` leaves,
/// bound per `[firecracker] cgroup_cpuset_topology`.
/// Best-effort: an unprivileged server that cannot create the base stays
/// quiet here, and the per-spawn attach warns once with the setup-host hint.
pub(super) fn ensure_runtime_base(pool_size: usize, topology: CpusetTopology) {
    let Some(root) = cgroup_v2_root() else {
        warn!("no cgroup v2 hierarchy; per-firecracker memcg isolation is unavailable");
        return;
    };
    match prepare_base(root) {
        Ok(base) => {
            // The leaf pool lived in the previous process's memory; reclaim
            // the non-pooled leaves it left behind, then reuse or create the
            // pooled slots before spawns begin.
            remove_unoccupied_leaves(pool_size);
            ensure_leaf_pool(pool_size, topology);
            info!(
                path = %base.display(),
                "firecracker cgroup base is ready"
            )
        }
        Err(err) => warn!(
            "could not provision the firecracker cgroup base at startup ({err:#}); \
             per-sandbox memcg isolation stays off until the server starts \
             privileged or `server --setup-host` runs as root"
        ),
    }
}

/// Creates the base and enables the memory controller down to it, then the
/// cpuset controller when the kernel offers it so pooled leaves can carry
/// `cpuset.cpus`/`cpuset.mems` masks.
fn prepare_base(root: &Path) -> Result<PathBuf> {
    let base = base_dir(root);
    fs::create_dir_all(&base)
        .with_context(|| format!("create firecracker cgroup base {}", base.display()))?;
    enable_controller(root, &base, MEMORY_CONTROLLER)?;
    // Best-effort: a host without cpuset (or an unprivileged write that
    // fails) just leaves the pool unbound, so this reports rather than
    // errors.
    if let Err(err) = enable_controller(root, &base, CPUSET_CONTROLLER) {
        info!(
            err = %err,
            "cpuset control unavailable; firecracker cgroup shards cannot be bound to a CPU topology"
        );
    }
    Ok(base)
}

/// Enables one controller in the root and base `cgroup.subtree_control` so
/// leaves under the base can use it. The root is exempt from the
/// no-internal-process rule and the base only ever holds leaves, so both
/// writes are legal. Errors name the level that failed; callers decide
/// whether that is fatal.
fn enable_controller(root: &Path, base: &Path, controller: &str) -> Result<()> {
    for dir in [root, base] {
        let controllers_path = dir.join("cgroup.controllers");
        let available = fs::read_to_string(&controllers_path)
            .with_context(|| format!("read {}", controllers_path.display()))?
            .split_whitespace()
            .any(|c| c == controller);
        anyhow::ensure!(
            available,
            "{controller} controller is not available in {}",
            dir.display()
        );
        let subtree = dir.join("cgroup.subtree_control");
        let enabled = fs::read_to_string(&subtree)
            .with_context(|| format!("read {}", subtree.display()))?
            .split_whitespace()
            .any(|c| c == controller);
        if !enabled {
            fs::write(&subtree, format!("+{controller}"))
                .with_context(|| format!("enable {controller} control in {}", subtree.display()))?;
        }
    }
    Ok(())
}

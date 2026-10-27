//! Proxy sandbox binding table (read-path design §3.2, §5.2, §5.3).
//!
//! A paused sandbox's rootfs is exposed to the resident proxy sandbox by
//! *binding* it to one of the proxy's deferred slot drives. The slot's device
//! node inside the proxy VM is fixed at boot — Firecracker cannot hot-plug —
//! so binding swaps the overlaybd stack served behind that node
//! (`AdoptSlotTarget`) and mounts it in the proxy guest.
//!
//! Lifecycle:
//!
//! * **BIND** on a target's first pause: allocate a free slot, create a
//!   read-only device from the staged snapshot config, adopt it into the slot,
//!   `mount -o ro,noload` in the proxy guest.
//! * **REBIND** on every later pause: same, but the guest mount stays put and
//!   only the caches are dropped.
//! * **UNBIND** when the target is deleted: `umount`, release the slot's
//!   adopted stack, free the slot. Runs before paused-artifact cleanup so the
//!   slot never references deleted layer files.
//!
//! Binding runs **asynchronously** after `pause` returns: the pause hook only
//! records the new generation, so pause latency is unaffected. Until the
//! binding catches up, reads report `rebind_in_progress` rather than serving a
//! stale generation.
//!
//! The bind/rebind steps are free functions rather than table methods: they run
//! on spawned tasks, and keeping the table out of their signature avoids
//! borrowing it across awaits (which costs the futures their `Send`ness).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use tokio::sync::{Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use uvm_ublk_daemon::CreateOverlaybdRuntimeDeviceRequest;

use crate::cfg::ConfigManager;
use crate::sandbox::{ProcessOpts, UblkDeviceManager};
use crate::types::SandboxId;

use super::proxy_pool::{proxy_slot_device_name, PROXY_TARGET_MOUNT_ROOT};
use super::types::SandboxState;

/// Guest-side toolbox shipped on the tools drive. Every proxy guest operation
/// (mount, umount, drop_caches, chroot) is a busybox applet, so no envd
/// protocol change or tools-drive rebuild is required.
const BUSYBOX: &str = "/agentenv/bin/busybox";

/// Busybox shell applet, used where a shell is needed (`drop_caches`
/// redirection). `/bin/sh` inside a chroot belongs to the target image instead.
const BUSYBOX_SHELL: &str = "hush";

/// Upper bound for waiting on in-flight proxied commands before a rebind. Each
/// command is bounded by its own timeout, so this is a safety net.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Default and maximum execution timeout for a proxied read command.
const DEFAULT_PROXY_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROXY_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Replacement character produced by lossy UTF-8 decoding. Its presence means
/// the command emitted bytes that are not valid UTF-8 (binary output), which
/// the read path refuses to forward as text.
const LOSSY_REPLACEMENT: char = '\u{FFFD}';

/// Machine-readable failure tags for the `proxy-read` endpoint. The server logs
/// the same strings, so client- and server-side logs can be correlated.
#[derive(Debug)]
pub enum ProxyReadFailure {
    /// Target sandbox does not exist.
    TargetNotFound,
    /// Target exists but is not Paused. Serving it would return a stale
    /// snapshot, so this is a caller contract violation.
    TargetNotPaused(SandboxState),
    /// Target is Paused but has no binding entry (mapping cleared by a server
    /// restart, or the proxy that held it is gone).
    BindingMissing,
    /// Binding for the current pause generation is still being established.
    RebindInProgress,
    /// Binding for the current pause generation failed.
    RebindFailed(String),
    /// Proxy sandbox is unavailable (deleted, not running, envd unreachable).
    ProxyUnavailable(String),
    /// Command output was not valid UTF-8.
    OutputNotUtf8,
    /// Command execution failed (timeout, envd error).
    ExecutionFailed(String),
}

impl ProxyReadFailure {
    /// Stable error tag shared with server logs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::TargetNotFound => "target_not_found",
            Self::TargetNotPaused(_) => "target_not_paused",
            Self::BindingMissing => "proxy_binding_missing",
            Self::RebindInProgress => "rebind_in_progress",
            Self::RebindFailed(_) => "rebind_failed",
            Self::ProxyUnavailable(_) => "proxy_unavailable",
            Self::OutputNotUtf8 => "output_not_utf8",
            Self::ExecutionFailed(_) => "proxy_exec_failed",
        }
    }

    /// HTTP status the API layer should return.
    pub fn status(&self) -> u16 {
        match self {
            Self::TargetNotFound => 404,
            Self::TargetNotPaused(_) => 409,
            Self::OutputNotUtf8 => 422,
            Self::BindingMissing => 500,
            Self::RebindInProgress | Self::RebindFailed(_) | Self::ProxyUnavailable(_) => 503,
            Self::ExecutionFailed(_) => 500,
        }
    }
}

impl std::fmt::Display for ProxyReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotFound => f.write_str("target sandbox not found"),
            Self::TargetNotPaused(state) => {
                write!(f, "target sandbox is not paused (state: {state})")
            }
            Self::BindingMissing => {
                f.write_str("target sandbox has no proxy binding for its rootfs")
            }
            Self::RebindInProgress => {
                f.write_str("proxy binding for this pause generation is still in progress")
            }
            Self::RebindFailed(reason) => {
                write!(
                    f,
                    "proxy binding for this pause generation failed: {reason}"
                )
            }
            Self::ProxyUnavailable(reason) => write!(f, "proxy sandbox unavailable: {reason}"),
            Self::OutputNotUtf8 => f.write_str("command output is not valid UTF-8"),
            Self::ExecutionFailed(reason) => write!(f, "proxied command failed: {reason}"),
        }
    }
}

/// A validated `proxy-read` request.
#[derive(Clone, Debug)]
pub struct ProxyReadCommand {
    /// Shell command, executed inside the target's chroot.
    pub command: String,
    /// Working directory inside the target's filesystem.
    pub cwd: Option<String>,
    /// Highest-priority environment layer.
    pub envs: HashMap<String, String>,
    pub timeout: Option<Duration>,
}

/// Result of a proxied read command.
#[derive(Clone, Debug)]
pub struct ProxyReadOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub binding_generation: u64,
}

/// Binding status of one target sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BindingStatus {
    /// A bind/rebind task is running for the latest generation.
    Binding,
    /// The slot serves `bound_gen`; readable when `bound_gen == latest_gen`.
    Bound,
    /// The latest bind/rebind failed; the slot may hold an older stack.
    Failed(String),
}

/// Mutable binding bookkeeping.
#[derive(Debug)]
struct BindingState {
    status: BindingStatus,
    /// Generation the slot currently serves.
    bound_gen: u64,
    /// Generation of the target's latest completed pause.
    latest_gen: u64,
    /// Daemon-materialized runtime directory of the adopted stack. Removed once
    /// a newer generation is adopted (or on unbind), never while it is served.
    runtime_dir: Option<PathBuf>,
    /// Environment recorded in the paused runtime; the base layer of the
    /// proxied command's environment (read-path design §5.3.3).
    env_vars: HashMap<String, String>,
    /// Working directory recorded for the target sandbox.
    workdir: Option<String>,
}

impl BindingState {
    fn env_vars(&self) -> &HashMap<String, String> {
        &self.env_vars
    }

    fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref()
    }
}

/// Shared state of one target's binding.
pub(crate) struct BindingEntry {
    target: SandboxId,
    proxy_id: SandboxId,
    slot_index: usize,
    slot_dev_id: u32,
    /// Serializes bind/rebind/unbind for this target. `Arc` so the guard is
    /// owned: a borrowed guard would put a `&Mutex<()>` in the future and cost
    /// it `Send`.
    bind_lock: Arc<Mutex<()>>,
    state: Arc<RwLock<BindingState>>,
    /// In-flight proxied commands; rebinds wait for this to reach zero.
    inflight: AtomicUsize,
    /// Signalled when the in-flight count drops; drained via a short sleep
    /// loop rather than a borrowed `Notified` future so the binding task stays
    /// `Send`.
    inflight_idle: Arc<Notify>,
    /// Set when the target is being deleted or its proxy disappeared, so
    /// pending bind work aborts instead of racing the teardown.
    cancelled: AtomicBool,
}

impl BindingEntry {
    /// Proxy sandbox currently holding this target's slot.
    pub(crate) fn proxy_id(&self) -> SandboxId {
        self.proxy_id
    }

    fn mount_path(&self) -> String {
        format!("{}/{}", PROXY_TARGET_MOUNT_ROOT, self.target)
    }

    fn guest_device(&self) -> String {
        format!("/dev/{}", proxy_slot_device_name(self.slot_index))
    }

    /// Readability gate: only a fully bound, generation-matching slot may serve
    /// commands. Anything else is an explicit error — never a stale generation.
    fn readable_generation(&self, state: &BindingState) -> Result<u64, BindingNotReady> {
        match &state.status {
            BindingStatus::Bound if state.bound_gen == state.latest_gen => Ok(state.bound_gen),
            BindingStatus::Bound | BindingStatus::Binding => Err(BindingNotReady::InProgress),
            BindingStatus::Failed(reason) => Err(BindingNotReady::Failed(reason.clone())),
        }
    }

    /// Mark a command as in flight. The returned guard decrements on drop, so a
    /// concurrent rebind drains this command instead of swapping the stack
    /// underneath it.
    ///
    /// Takes the `Arc` (not `&self`) so the guard does not borrow the entry
    /// across the command's awaits.
    fn enter_command(self: &Arc<Self>) -> InFlightGuard {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        InFlightGuard {
            entry: Arc::clone(self),
        }
    }
}

/// Why a bound target cannot serve a read right now.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BindingNotReady {
    InProgress,
    Failed(String),
}

/// RAII decrement for the per-target in-flight counter.
struct InFlightGuard {
    entry: Arc<BindingEntry>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.entry.inflight.fetch_sub(1, Ordering::AcqRel);
        self.entry.inflight_idle.notify_waiters();
    }
}

/// One registered proxy sandbox and its slot inventory.
struct ProxyNode {
    /// ublk dev_id per slot index (slot order == deferred drive order).
    slot_dev_ids: Vec<u32>,
    /// Free slot indices. Popped from the front under the mutex so two
    /// concurrent first-pauses cannot claim the same slot.
    free: Mutex<VecDeque<usize>>,
}

/// Inputs for one bind/rebind attempt, captured synchronously by the pause hook.
pub(crate) struct ProxyBindRequest {
    /// Staged snapshot rootfs config produced by this pause.
    pub image_config_path: PathBuf,
    pub runtime_upper_mode: overlaybd::config::UpperMode,
    /// Environment recorded in the paused runtime.
    pub env_vars: HashMap<String, String>,
    /// Working directory recorded for the target sandbox.
    pub workdir: Option<String>,
}

/// Outcome of the pause hook's synchronous segment.
pub(crate) struct PauseHookBinding {
    pub(crate) entry: Arc<BindingEntry>,
    pub(crate) generation: u64,
}

/// Why the pause hook did not schedule proxy-side work.
#[derive(Debug)]
pub(crate) enum PauseHookSkip {
    /// The target is being torn down.
    Cancelled,
    /// No free slot exists on any registered proxy.
    NoFreeSlot,
    /// No proxy sandbox is registered (proxy disabled or still starting).
    NoProxy,
}

impl std::fmt::Display for PauseHookSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("target is being torn down"),
            Self::NoFreeSlot => f.write_str("no free proxy slot"),
            Self::NoProxy => f.write_str("no proxy sandbox registered"),
        }
    }
}

/// Orchestrator-owned proxy binding registry.
///
/// In-memory only: after a server restart the table is empty, so paused targets
/// report a missing binding until their next pause re-binds them.
#[derive(Default)]
pub(crate) struct ProxyBindingTable {
    targets: DashMap<SandboxId, Arc<BindingEntry>>,
    proxies: DashMap<SandboxId, Arc<ProxyNode>>,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<ProxyBindingTable>();
    assert_send_sync::<BindingEntry>();
    assert_send_sync::<ProxyNode>();
    assert_send_sync::<ProxyExecutor>();
};

impl ProxyBindingTable {
    /// Register a running proxy sandbox and make its deferred slot drives
    /// allocatable.
    pub(crate) fn register_proxy(&self, proxy_id: SandboxId, slot_dev_ids: Vec<u32>) {
        let slot_count = slot_dev_ids.len();
        let free: VecDeque<usize> = (0..slot_count).collect();
        self.proxies.insert(
            proxy_id,
            Arc::new(ProxyNode {
                slot_dev_ids,
                free: Mutex::new(free),
            }),
        );
        info!(
            proxy_id = %proxy_id,
            slots = slot_count,
            "registered proxy sandbox slots"
        );
    }

    /// Drop a proxy and every binding it held. The proxy VM is gone, so its
    /// guest mounts are gone with it; affected targets become unreadable until
    /// their next pause rebinds them onto another proxy.
    pub(crate) fn unregister_proxy(&self, proxy_id: SandboxId) {
        let Some((_, node)) = self.proxies.remove(&proxy_id) else {
            return;
        };
        let slot_count = node.slot_dev_ids.len();
        let mut affected = 0usize;
        self.targets.retain(|_, entry| {
            if entry.proxy_id == proxy_id {
                entry.cancelled.store(true, Ordering::Release);
                affected += 1;
                false
            } else {
                true
            }
        });
        warn!(
            proxy_id = %proxy_id,
            slots = slot_count,
            affected_targets = affected,
            "proxy sandbox unregistered; its bindings are gone"
        );
    }

    /// Number of registered target bindings (observability).
    pub(crate) fn binding_count(&self) -> usize {
        self.targets.len()
    }

    /// Look up a target's binding entry.
    fn get(&self, target: &SandboxId) -> Option<Arc<BindingEntry>> {
        self.targets.get(target).map(|item| Arc::clone(&item))
    }

    /// Pause-hook synchronous segment: record that `target` reached a new pause
    /// generation, reserving a slot when this is its first pause.
    ///
    /// Cheap and non-blocking (no device or guest work); the caller then spawns
    /// the asynchronous bind. Pause itself never fails because of this.
    pub(crate) async fn record_pause_generation(
        &self,
        target: SandboxId,
    ) -> Result<PauseHookBinding, PauseHookSkip> {
        if let Some(entry) = self.get(&target) {
            if entry.cancelled.load(Ordering::Acquire) {
                return Err(PauseHookSkip::Cancelled);
            }
            let generation = {
                let mut state = Arc::clone(&entry.state).write_owned().await;
                state.latest_gen += 1;
                state.status = BindingStatus::Binding;
                state.latest_gen
            };
            return Ok(PauseHookBinding { entry, generation });
        }

        let Some((proxy_id, node, slot_index)) = self.allocate_slot().await else {
            return Err(if self.proxies.is_empty() {
                PauseHookSkip::NoProxy
            } else {
                PauseHookSkip::NoFreeSlot
            });
        };
        let slot_dev_id = node.slot_dev_ids[slot_index];
        let entry = Arc::new(BindingEntry {
            target,
            proxy_id,
            slot_index,
            slot_dev_id,
            bind_lock: Arc::new(Mutex::new(())),
            state: Arc::new(RwLock::new(BindingState {
                status: BindingStatus::Binding,
                bound_gen: 0,
                latest_gen: 1,
                runtime_dir: None,
                env_vars: HashMap::new(),
                workdir: None,
            })),
            inflight: AtomicUsize::new(0),
            inflight_idle: Arc::new(Notify::new()),
            cancelled: AtomicBool::new(false),
        });

        match self.targets.entry(target) {
            dashmap::mapref::entry::Entry::Occupied(occupied) => {
                // Lost a race with a concurrent first pause: give the slot back
                // and adopt the winner's entry.
                let existing = Arc::clone(occupied.get());
                drop(occupied);
                self.release_slot(&proxy_id, slot_index).await;
                if existing.cancelled.load(Ordering::Acquire) {
                    return Err(PauseHookSkip::Cancelled);
                }
                let generation = {
                    let mut state = Arc::clone(&existing.state).write_owned().await;
                    state.latest_gen += 1;
                    state.status = BindingStatus::Binding;
                    state.latest_gen
                };
                Ok(PauseHookBinding {
                    entry: existing,
                    generation,
                })
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(Arc::clone(&entry));
                debug!(
                    target = %target,
                    proxy_id = %proxy_id,
                    slot_index,
                    slot_dev_id,
                    "allocated proxy slot for first pause"
                );
                Ok(PauseHookBinding {
                    entry,
                    generation: 1,
                })
            }
        }
    }

    async fn allocate_slot(&self) -> Option<(SandboxId, Arc<ProxyNode>, usize)> {
        // Snapshot the proxy ids first: holding a DashMap guard while taking the
        // slot mutex would nest locks with unregister_proxy.
        let proxy_ids: Vec<SandboxId> = self.proxies.iter().map(|item| *item.key()).collect();
        for proxy_id in proxy_ids {
            let node = self.proxies.get(&proxy_id).map(|item| Arc::clone(&item))?;
            let slot_index = {
                let mut free = node.free.lock().await;
                free.pop_front()
            };
            if let Some(slot_index) = slot_index {
                return Some((proxy_id, node, slot_index));
            }
        }
        None
    }

    async fn release_slot(&self, proxy_id: &SandboxId, slot_index: usize) {
        if let Some(node) = self.proxies.get(proxy_id).map(|item| Arc::clone(&item)) {
            let mut free = node.free.lock().await;
            if !free.contains(&slot_index) {
                free.push_back(slot_index);
            }
        }
    }

    /// Tear down a target's binding: umount in the proxy, release the slot's
    /// adopted stack, free the slot, drop the entry.
    ///
    /// Errors propagate — the caller (delete) must not clean up paused
    /// artifacts while the slot may still reference their layer files.
    pub(crate) async fn unbind_target(
        self: Arc<Self>,
        target: SandboxId,
        proxy_executor: ProxyExecutor,
    ) -> Result<()> {
        let Some((_, entry)) = self.targets.remove(&target) else {
            return Ok(());
        };
        // Runs on a blocking thread for the same reason as `spawn_bind`.
        let handle = tokio::runtime::Handle::current();
        let this = Arc::clone(&self);
        tokio::task::spawn_blocking(move || {
            handle.block_on(this.unbind_target_inner(entry, target, proxy_executor))
        })
        .await
        .context("join proxy unbind task")?
    }

    async fn unbind_target_inner(
        self: Arc<Self>,
        entry: Arc<BindingEntry>,
        target: SandboxId,
        proxy_executor: ProxyExecutor,
    ) -> Result<()> {
        entry.cancelled.store(true, Ordering::Release);

        let _serialized = Arc::clone(&entry.bind_lock).lock_owned().await;
        drain_inflight(Arc::clone(&entry)).await;

        let had_binding = {
            let state = Arc::clone(&entry.state).read_owned().await;
            state.bound_gen != 0
        };
        let mut first_error: Option<anyhow::Error> = None;

        // Guest-side teardown. A dead proxy has no mounts left to remove, so an
        // unreachable executor is logged rather than fatal — the slot release
        // below is what actually frees resources.
        let proxy_id = entry.proxy_id;
        if had_binding {
            if let Err(err) = umount_in_proxy(Arc::clone(&entry), proxy_executor).await {
                warn!(
                    target = %target,
                    proxy_id = %proxy_id,
                    error = %format_args!("{err:#}"),
                    "proxy guest umount failed during unbind; continuing with slot release"
                );
                first_error.get_or_insert(err);
            }
        }

        if let Err(err) = UblkDeviceManager::global()
            .unbind_slot(entry.slot_dev_id)
            .await
        {
            error!(
                target = %target,
                slot_dev_id = entry.slot_dev_id,
                error = %format_args!("{err:#}"),
                "proxy_unbind_failed: slot may still reference the target's layer files"
            );
            first_error.get_or_insert(err.context("release proxy slot binding"));
        }

        let runtime_dir = {
            let mut state = Arc::clone(&entry.state).write_owned().await;
            state.runtime_dir.take()
        };
        if let Some(runtime_dir) = runtime_dir {
            cleanup_runtime_dir(runtime_dir).await;
        }

        let (proxy_id, slot_index) = (entry.proxy_id, entry.slot_index);
        self.release_slot(&proxy_id, slot_index).await;
        info!(
            target = %target,
            proxy_id = %proxy_id,
            slot_index,
            "proxy binding released"
        );

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Execute a read command against a bound target's rootfs inside the proxy.
    ///
    /// The caller has already verified the target is Paused. Any unreadable
    /// state is an explicit [`ProxyReadFailure`]: a stale generation is never
    /// served silently.
    /// The binding entry for a target, when one exists.
    ///
    /// Exposed so the orchestrator can resolve the proxy handle (and report
    /// `proxy_unavailable`) before the generation check.
    pub(crate) fn proxy_entry(&self, target: &SandboxId) -> Option<Arc<BindingEntry>> {
        self.get(target)
    }

    /// Whether `sandbox_id` is a registered proxy sandbox.
    pub(crate) fn is_proxy(&self, sandbox_id: &SandboxId) -> bool {
        self.proxies.contains_key(sandbox_id)
    }

    /// Run a read command against an already-resolved proxy executor.
    ///
    /// Driven on a blocking thread (like [`spawn_bind`]): the command execution
    /// borrows argv across an await, and confining that to its own task keeps
    /// the caller's future `Send` without fighting auto-trait inference.
    pub(crate) async fn proxy_read(
        self: &Arc<Self>,
        target: SandboxId,
        command: ProxyReadCommand,
        executor: crate::sandbox::Executor,
    ) -> Result<ProxyReadOutput, ProxyReadFailure> {
        let this = Arc::clone(self);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(this.proxy_read_inner(target, command, executor))
        })
        .await
        .map_err(|err| {
            ProxyReadFailure::ExecutionFailed(format!("proxy read task panicked: {err}"))
        })?
    }

    async fn proxy_read_inner(
        self: Arc<Self>,
        target: SandboxId,
        command: ProxyReadCommand,
        executor: crate::sandbox::Executor,
    ) -> Result<ProxyReadOutput, ProxyReadFailure> {
        let entry = self.get(&target).ok_or(ProxyReadFailure::BindingMissing)?;

        let (generation, envs, cwd) = {
            let state = Arc::clone(&entry.state).read_owned().await;
            let generation =
                entry
                    .readable_generation(&state)
                    .map_err(|not_ready| match not_ready {
                        BindingNotReady::InProgress => ProxyReadFailure::RebindInProgress,
                        BindingNotReady::Failed(reason) => ProxyReadFailure::RebindFailed(reason),
                    })?;
            // Three-layer merge: the target's recorded environment first, then
            // per-request overrides.
            let mut envs = state.env_vars().clone();
            envs.extend(command.envs.clone());
            let cwd = command
                .cwd
                .clone()
                .or_else(|| state.workdir().map(str::to_string))
                .unwrap_or_else(|| "/".to_string());
            (generation, envs, cwd)
        };

        let mount_path = entry.mount_path();
        let script = build_chroot_script(&command, &cwd);
        let timeout = command
            .timeout
            .unwrap_or(DEFAULT_PROXY_READ_TIMEOUT)
            .min(MAX_PROXY_READ_TIMEOUT);

        let (proxy_id, slot_index) = (entry.proxy_id, entry.slot_index);
        let args: Vec<&str> = vec!["chroot", &mount_path, "/bin/sh", "-c", &script];
        let opts = ProcessOpts::new().with_envs(envs).with_timeout(timeout);

        let _in_flight = Arc::clone(&entry).enter_command();
        let started = Instant::now();
        let result = executor.run_command_with_opts(BUSYBOX, &args, &opts).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        drop(_in_flight);

        let output = match result {
            Ok(output) => output,
            Err(err) => {
                let message = format!("{err:#}");
                error!(
                    target = %target,
                    proxy_id = %proxy_id,
                    slot_index,
                    generation,
                    error = %message,
                    "proxy_read_exec_failed"
                );
                return Err(ProxyReadFailure::ExecutionFailed(message));
            }
        };

        if output.stdout.contains(LOSSY_REPLACEMENT) || output.stderr.contains(LOSSY_REPLACEMENT) {
            warn!(
                target = %target,
                generation,
                "proxy_read_output_not_utf8: refusing to return lossy command output"
            );
            return Err(ProxyReadFailure::OutputNotUtf8);
        }

        Ok(ProxyReadOutput {
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms,
            binding_generation: generation,
        })
    }
}

/// Bind or rebind a target to its recorded pause generation.
///
/// Runs on a background task spawned by the pause hook. Failures are logged at
/// `error` and recorded on the entry so reads report `rebind_failed` instead of
/// silently serving stale data.
/// Spawn the bind/rebind task for one target.
///
/// The work runs on a blocking thread driving its own future, the same pattern
/// `FirecrackerSandbox::sync_writable_volume_filesystems` uses. Several steps
/// below hold guards or borrows across awaits (ublk RPCs, envd commands);
/// confining them to a dedicated task keeps the future handed to `tokio::spawn`
/// trivially `Send` instead of relying on the compiler generalizing those
/// lifetimes.
pub(crate) fn spawn_bind(
    entry: Arc<BindingEntry>,
    request: ProxyBindRequest,
    proxy_executor: ProxyExecutor,
) {
    let handle = tokio::runtime::Handle::current();
    // Detached on purpose: the pause hook returns immediately while the binding
    // continues; failures are logged inside `run_bind`.
    std::mem::drop(tokio::task::spawn_blocking(move || {
        handle.block_on(run_bind(entry, request, proxy_executor))
    }));
}

async fn run_bind(
    entry: Arc<BindingEntry>,
    request: ProxyBindRequest,
    proxy_executor: ProxyExecutor,
) {
    let target = entry.target;
    let _serialized = Arc::clone(&entry.bind_lock).lock_owned().await;
    if entry.cancelled.load(Ordering::Acquire) {
        debug!(target = %target, "proxy binding cancelled before bind");
        return;
    }

    let first_bind = {
        let state = Arc::clone(&entry.state).read_owned().await;
        state.bound_gen == 0
    };
    drain_inflight(Arc::clone(&entry)).await;
    {
        let mut state = Arc::clone(&entry.state).write_owned().await;
        state.env_vars = request.env_vars.clone();
        state.workdir = request.workdir.clone();
    }
    let result = if first_bind {
        bind(Arc::clone(&entry), request, proxy_executor).await
    } else {
        rebind(Arc::clone(&entry), request, proxy_executor).await
    };

    match result {
        Ok(generation) => {
            let mut state = Arc::clone(&entry.state).write_owned().await;
            state.status = BindingStatus::Bound;
            state.bound_gen = generation;
            info!(
                target = %target,
                proxy_id = %entry.proxy_id,
                slot_index = entry.slot_index,
                generation,
                first_bind,
                "proxy binding ready"
            );
        }
        Err(err) => {
            let reason = format!("{err:#}");
            let generation = {
                let state = Arc::clone(&entry.state).read_owned().await;
                state.latest_gen
            };
            error!(
                target = %target,
                proxy_id = %entry.proxy_id,
                slot_index = entry.slot_index,
                slot_dev_id = entry.slot_dev_id,
                generation,
                first_bind,
                error = %reason,
                "proxy_bind_failed"
            );
            let mut state = Arc::clone(&entry.state).write_owned().await;
            state.status = BindingStatus::Failed(reason);
        }
    }
}

/// Create the read-only device for the staged config, adopt it into the slot,
/// then mount it in the proxy guest (first pause only).
async fn bind(
    entry: Arc<BindingEntry>,
    request: ProxyBindRequest,
    proxy_executor: ProxyExecutor,
) -> Result<u64> {
    let generation = {
        let state = Arc::clone(&entry.state).read_owned().await;
        state.latest_gen
    };
    let runtime_dir = create_runtime_device(Arc::clone(&entry), request, generation).await?;

    if let Err(err) = mount_in_proxy(Arc::clone(&entry), proxy_executor).await {
        // Roll back so neither the slot nor the runtime directory leaks.
        release_slot_device(Arc::clone(&entry)).await;
        cleanup_runtime_dir(runtime_dir).await;
        return Err(err.context("mount bound rootfs in proxy guest"));
    }

    let mut state = Arc::clone(&entry.state).write_owned().await;
    state.runtime_dir = Some(runtime_dir);
    Ok(generation)
}

/// Adopt the new generation's stack into the already-mounted slot, drop guest
/// caches, then remove the previous generation's runtime directory.
async fn rebind(
    entry: Arc<BindingEntry>,
    request: ProxyBindRequest,
    proxy_executor: ProxyExecutor,
) -> Result<u64> {
    let (generation, previous_runtime_dir) = {
        let state = Arc::clone(&entry.state).read_owned().await;
        (state.latest_gen, state.runtime_dir.clone())
    };
    let runtime_dir = create_runtime_device(Arc::clone(&entry), request, generation).await?;

    if let Err(err) = remount_in_proxy(Arc::clone(&entry), proxy_executor).await {
        // The slot already serves the new stack, but the guest would keep
        // reading the previous generation through the stale mount: treat it
        // as a hard failure. The runtime directory stays referenced by the
        // entry so the next attempt can supersede it.
        let mut state = Arc::clone(&entry.state).write_owned().await;
        state.runtime_dir = Some(runtime_dir);
        return Err(err.context("remount bound rootfs in proxy guest"));
    }

    if let Some(previous) = previous_runtime_dir {
        // Safe now: the adopt swapped the slot's image, so nothing references
        // the previous runtime config any more.
        cleanup_runtime_dir(previous).await;
    }

    {
        let mut state = Arc::clone(&entry.state).write_owned().await;
        state.runtime_dir = Some(runtime_dir);
    }
    Ok(generation)
}

/// Materialize a read-only runtime device from the staged snapshot config and
/// adopt it into the slot.
async fn create_runtime_device(
    entry: Arc<BindingEntry>,
    request: ProxyBindRequest,
    generation: u64,
) -> Result<PathBuf> {
    let global_config = ConfigManager::global_config();
    let runtime_dir = proxy_binding_runtime_dir(entry.target, generation);
    if let Some(parent) = runtime_dir.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }

    let created = UblkDeviceManager::global()
        .create_overlaybd_runtime_device(CreateOverlaybdRuntimeDeviceRequest {
            source_image_config: &request.image_config_path,
            global_config: &global_config.ublk.overlaybd.global_config_path,
            runtime_dir: &runtime_dir,
            // Read-only is the safety boundary: writes fail with EROFS instead
            // of flowing into the target's layers.
            read_only: true,
            runtime_upper_mode: request.runtime_upper_mode,
            requested_virtual_size: None,
            known_source_virtual_size: None,
            allow_shrink: false,
        })
        .await
        .with_context(|| {
            format!(
                "create read-only overlaybd device from staged config {}",
                request.image_config_path.display()
            )
        })?;

    let source_dev_id = created.device.dev_id();
    if let Err(err) = UblkDeviceManager::global()
        .adopt_slot_target(entry.slot_dev_id, source_dev_id)
        .await
    {
        // Adopt failed: the source device is still ours to release and the
        // runtime directory is unreferenced.
        if let Err(release_err) = UblkDeviceManager::global()
            .release_device_by_id(source_dev_id)
            .await
        {
            warn!(
                source_dev_id,
                error = %format_args!("{release_err:#}"),
                "failed to release source device after adopt failure"
            );
        }
        cleanup_runtime_dir(runtime_dir).await;
        return Err(err.context("adopt staged stack into proxy slot"));
    }

    Ok(runtime_dir)
}

/// Release whatever stack the slot currently holds (best effort).
async fn release_slot_device(entry: Arc<BindingEntry>) {
    if let Err(err) = UblkDeviceManager::global()
        .unbind_slot(entry.slot_dev_id)
        .await
    {
        warn!(
            slot_dev_id = entry.slot_dev_id,
            error = %format_args!("{err:#}"),
            "failed to release proxy slot device after bind failure"
        );
    }
}

async fn cleanup_runtime_dir(runtime_dir: PathBuf) {
    if let Err(err) = tokio::fs::remove_dir_all(&runtime_dir).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!(
                path = %runtime_dir.display(),
                error = %err,
                "failed to remove proxy binding runtime directory"
            );
        }
    }
}

/// Guest-side mount of a freshly bound slot (BIND only).
async fn mount_in_proxy(entry: Arc<BindingEntry>, proxy_executor: ProxyExecutor) -> Result<()> {
    let executor = proxy_executor.get().await?;
    let mount_path = entry.mount_path();
    let device = entry.guest_device();

    let mkdir = executor.clone();
    run_busybox(mkdir, owned(&["mkdir", "-p", &mount_path]))
        .await
        .context("create proxy mount point")?;
    // `noload` keeps the mount read-only in earnest: ext4 would otherwise
    // replay the journal, which is a write.
    let mount = executor.clone();
    run_busybox(
        mount,
        owned(&["mount", "-n", "-o", "ro,noload", &device, &mount_path]),
    )
    .await
    .with_context(|| format!("mount {device} at {mount_path}"))?;

    // Tools inspecting /proc or /dev inside the chroot need them present.
    for source in ["/proc", "/dev"] {
        let name = source.trim_start_matches('/');
        let target = format!("{mount_path}/{name}");
        let mkdir = executor.clone();
        if let Err(err) = run_busybox(mkdir, owned(&["mkdir", "-p", &target])).await {
            warn!(error = %format_args!("{err:#}"), "failed to create {target}");
        }
        let bind_mount = executor.clone();
        run_busybox(
            bind_mount,
            owned(&["mount", "-n", "--bind", source, &target]),
        )
        .await
        .with_context(|| format!("bind mount {source} at {target}"))?;
    }

    debug!(target = %entry.target, %mount_path, %device, "proxy slot mounted");
    Ok(())
}

/// Guest-side remount of an already-bound slot (REBIND).
///
/// The ext4 mount holds kernel-allocated state (in-memory superblock copy,
/// group descriptors) that `drop_caches` cannot refresh, so a rebind that
/// only drops caches deterministically serves the pre-rebind view for the
/// first rebind after BIND. Unmounting and re-mounting the slot device makes
/// the guest re-read every metadata block from the newly adopted stack.
async fn remount_in_proxy(entry: Arc<BindingEntry>, proxy_executor: ProxyExecutor) -> Result<()> {
    let executor = proxy_executor.get().await?;
    let mount_path = entry.mount_path();
    let device = entry.guest_device();

    // Detach the old view, submounts first. Lazy umount tolerates a busy
    // mount left by a command that outlived its timeout; the fresh mount
    // shadows whatever remains underneath.
    for target in [
        format!("{mount_path}/dev"),
        format!("{mount_path}/proc"),
        mount_path.clone(),
    ] {
        let umount = executor.clone();
        if let Err(err) = run_busybox(umount, owned(&["umount", "-l", &target])).await {
            debug!(
                %target,
                error = %format_args!("{err:#}"),
                "proxy guest remount umount reported an error"
            );
        }
    }

    let mount = executor.clone();
    run_busybox(
        mount,
        owned(&["mount", "-n", "-o", "ro,noload", &device, &mount_path]),
    )
    .await
    .with_context(|| format!("remount {device} at {mount_path}"))?;

    // Tools inspecting /proc or /dev inside the chroot need them present.
    for source in ["/proc", "/dev"] {
        let name = source.trim_start_matches('/');
        let target = format!("{mount_path}/{name}");
        let bind_mount = executor.clone();
        run_busybox(
            bind_mount,
            owned(&["mount", "-n", "--bind", source, &target]),
        )
        .await
        .with_context(|| format!("bind mount {source} at {target} during remount"))?;
    }

    debug!(target = %entry.target, %mount_path, %device, "proxy slot remounted");
    Ok(())
}

async fn umount_in_proxy(entry: Arc<BindingEntry>, proxy_executor: ProxyExecutor) -> Result<()> {
    let executor = proxy_executor.get().await?;
    let mount_path = entry.mount_path();
    // Lazy umount tolerates a busy mount left by a command that outlived its
    // timeout; the binding is being torn down either way.
    for target in [
        format!("{mount_path}/dev"),
        format!("{mount_path}/proc"),
        mount_path.clone(),
    ] {
        let umount = executor.clone();
        if let Err(err) = run_busybox(umount, owned(&["umount", "-l", &target])).await {
            debug!(
                %target,
                error = %format_args!("{err:#}"),
                "proxy guest umount reported an error"
            );
        }
    }
    let _ = run_busybox(
        executor,
        owned(&[BUSYBOX_SHELL, "-c", "echo 3 > /proc/sys/vm/drop_caches"]),
    )
    .await;
    Ok(())
}

/// Wait until no proxied command is in flight for this target.
///
/// Commands are short and self-bounded by their timeout, so this normally
/// returns immediately; the cap only stops a stuck command from blocking
/// rebinds forever.
async fn drain_inflight(entry: Arc<BindingEntry>) {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while entry.inflight.load(Ordering::Acquire) > 0 {
        if Instant::now() >= deadline {
            warn!(
                target = %entry.target,
                inflight = entry.inflight.load(Ordering::Acquire),
                "timed out draining in-flight proxy commands before rebind"
            );
            return;
        }
        tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
    }
}

/// Lazy, re-resolving accessor for the proxy sandbox's process executor.
///
/// The proxy handle lives in the orchestrator's table and may be replaced (e.g.
/// after the proxy restarts) or removed, so it is resolved at each use instead
/// of captured.
#[derive(Clone)]
pub(crate) struct ProxyExecutor {
    resolve: Arc<dyn Fn() -> ProxyExecutorFuture + Send + Sync>,
}

/// Boxed future returned by [`ProxyExecutor`].
type ProxyExecutorFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<crate::sandbox::Executor>> + Send + 'static>,
>;

impl ProxyExecutor {
    /// Build a resolver from an async closure over orchestrator state.
    pub(crate) fn new<F, Fut>(resolve: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<crate::sandbox::Executor>> + Send + 'static,
    {
        Self {
            resolve: Arc::new(move || Box::pin(resolve())),
        }
    }

    /// Resolve the executor now, failing when the proxy is unavailable.
    ///
    /// Takes `self` by value and returns a `'static` boxed future rather than
    /// being an `async fn`: a borrow of the resolver would otherwise live
    /// across the caller's await point and cost the enclosing future its
    /// `Send`ness.
    pub(crate) fn get(self) -> ProxyExecutorFuture {
        (self.resolve)()
    }
}

/// Build the shell script executed inside the target's chroot.
///
/// `cwd` becomes a `cd` prefix rather than a process working directory: chroot
/// does not change cwd, and an explicit `cd` fails loudly when the directory is
/// missing instead of silently running somewhere else.
fn build_chroot_script(command: &ProxyReadCommand, cwd: &str) -> String {
    format!("cd {} && {}", shell_util::shell_quote(cwd), command.command)
}

/// Copy borrowed args into owned strings.
///
/// Evaluating this *before* the `run_busybox` await ends the borrow of any
/// interpolated `String` (mount paths, device names), so none of it lives across
/// an await point inside the spawned binding task.
fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

/// Run a busybox applet in the proxy guest and fail on a non-zero exit.
///
/// Takes owned `String` args (not `&str`) so nothing borrowed lives across the
/// envd await, and a cloned executor handle so no `&Executor` borrow does
/// either — both keep the enclosing binding task `Send`.
async fn run_busybox(executor: crate::sandbox::Executor, args: Vec<String>) -> Result<()> {
    let applet = args.first().cloned().unwrap_or_default();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = executor
        .run_command_with_opts(BUSYBOX, &arg_refs, &ProcessOpts::new())
        .await
        .with_context(|| format!("run busybox {applet} in proxy guest"))?;
    if output.exit_code != 0 {
        bail!(
            "busybox {applet} failed in proxy guest (exit {}): {}",
            output.exit_code,
            output.stderr.trim()
        );
    }
    Ok(())
}

/// Daemon-materialized runtime directory for one binding generation.
fn proxy_binding_runtime_dir(target: SandboxId, generation: u64) -> PathBuf {
    ConfigManager::global_config()
        .firecracker
        .work_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("aenv"))
        .join("proxy-bindings")
        .join(target.to_string())
        .join(format!("gen-{generation}-{}", Uuid::now_v7().simple()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with_proxy(slots: usize) -> ProxyBindingTable {
        let table = ProxyBindingTable::default();
        table.register_proxy(
            SandboxId::new(),
            (0..slots).map(|index| index as u32 + 10).collect(),
        );
        table
    }

    #[tokio::test]
    async fn first_pause_allocates_a_slot_and_later_pauses_reuse_it() {
        let table = table_with_proxy(2);
        let target = SandboxId::new();

        let first = table
            .record_pause_generation(target)
            .await
            .expect("first pause");
        assert_eq!(first.generation, 1);
        let second = table
            .record_pause_generation(target)
            .await
            .expect("second pause");
        assert_eq!(second.generation, 2);
        assert_eq!(first.entry.slot_index, second.entry.slot_index);
        assert_eq!(table.binding_count(), 1);
    }

    #[tokio::test]
    async fn slots_are_handed_out_without_overlap_until_exhausted() {
        let table = table_with_proxy(2);
        let a = table
            .record_pause_generation(SandboxId::new())
            .await
            .unwrap();
        let b = table
            .record_pause_generation(SandboxId::new())
            .await
            .unwrap();
        assert_ne!(a.entry.slot_index, b.entry.slot_index);

        let third = table.record_pause_generation(SandboxId::new()).await;
        assert!(matches!(third, Err(PauseHookSkip::NoFreeSlot)));
    }

    #[tokio::test]
    async fn no_proxy_is_reported_as_no_proxy() {
        let table = ProxyBindingTable::default();
        let result = table.record_pause_generation(SandboxId::new()).await;
        assert!(matches!(result, Err(PauseHookSkip::NoProxy)));
    }

    #[tokio::test]
    async fn a_cancelled_entry_rejects_new_generations() {
        let table = table_with_proxy(1);
        let target = SandboxId::new();
        let binding = table.record_pause_generation(target).await.unwrap();
        binding.entry.cancelled.store(true, Ordering::Release);

        let result = table.record_pause_generation(target).await;
        assert!(matches!(result, Err(PauseHookSkip::Cancelled)));
    }

    #[tokio::test]
    async fn unreadable_until_bound_and_generation_matches() {
        let table = table_with_proxy(1);
        let target = SandboxId::new();
        let binding = table.record_pause_generation(target).await.unwrap();
        let entry = binding.entry;

        // Freshly recorded: binding in progress.
        {
            let state = Arc::clone(&entry.state).read_owned().await;
            assert!(matches!(
                entry.readable_generation(&state),
                Err(BindingNotReady::InProgress)
            ));
        }

        // Mark it bound for generation 1.
        {
            let mut state = Arc::clone(&entry.state).write_owned().await;
            state.status = BindingStatus::Bound;
            state.bound_gen = 1;
            assert_eq!(entry.readable_generation(&state), Ok(1));
        }

        // A newer pause without a completed rebind is not readable.
        table.record_pause_generation(target).await.unwrap();
        {
            let state = Arc::clone(&entry.state).read_owned().await;
            assert!(matches!(
                entry.readable_generation(&state),
                Err(BindingNotReady::InProgress)
            ));
        }

        // A failed bind surfaces its reason.
        {
            let mut state = Arc::clone(&entry.state).write_owned().await;
            state.status = BindingStatus::Failed("adopt failed".to_string());
            match entry.readable_generation(&state) {
                Err(BindingNotReady::Failed(reason)) => assert_eq!(reason, "adopt failed"),
                other => panic!("expected failure, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn released_slot_is_reusable_and_never_double_counted() {
        let table = table_with_proxy(1);
        let proxy_id = *table.proxies.iter().next().unwrap().key();
        let binding = table
            .record_pause_generation(SandboxId::new())
            .await
            .unwrap();
        let slot_index = binding.entry.slot_index;

        // Releasing twice must not hand the same slot out twice.
        table.release_slot(&proxy_id, slot_index).await;
        table.release_slot(&proxy_id, slot_index).await;

        let first = table
            .record_pause_generation(SandboxId::new())
            .await
            .expect("slot free");
        assert!(table
            .record_pause_generation(SandboxId::new())
            .await
            .is_err());
        assert_eq!(first.entry.slot_index, slot_index);
    }

    #[tokio::test]
    async fn unregistering_a_proxy_drops_its_bindings() {
        let table = ProxyBindingTable::default();
        let proxy_id = SandboxId::new();
        table.register_proxy(proxy_id, vec![3]);
        let target = SandboxId::new();
        table.record_pause_generation(target).await.unwrap();

        table.unregister_proxy(proxy_id);
        assert_eq!(table.binding_count(), 0);
        assert!(table.get(&target).is_none());
    }

    #[test]
    fn chroot_script_quotes_cwd_and_keeps_the_command_verbatim() {
        let command = ProxyReadCommand {
            command: "cat -n /workspace/README.rst | sed -n '1,50p'".to_string(),
            cwd: Some("/workspace/it's here".to_string()),
            envs: HashMap::new(),
            timeout: None,
        };
        let script = build_chroot_script(&command, command.cwd.as_deref().unwrap());
        assert!(script.starts_with("cd '/workspace/it'\\''s here' && "));
        assert!(script.contains("cat -n /workspace/README.rst"));
    }

    #[test]
    fn failure_codes_and_statuses_are_stable() {
        let cases = [
            (ProxyReadFailure::TargetNotFound, "target_not_found", 404),
            (
                ProxyReadFailure::TargetNotPaused(SandboxState::Running),
                "target_not_paused",
                409,
            ),
            (ProxyReadFailure::OutputNotUtf8, "output_not_utf8", 422),
            (
                ProxyReadFailure::BindingMissing,
                "proxy_binding_missing",
                500,
            ),
            (
                ProxyReadFailure::RebindInProgress,
                "rebind_in_progress",
                503,
            ),
            (
                ProxyReadFailure::RebindFailed("x".to_string()),
                "rebind_failed",
                503,
            ),
            (
                ProxyReadFailure::ProxyUnavailable("x".to_string()),
                "proxy_unavailable",
                503,
            ),
            (
                ProxyReadFailure::ExecutionFailed("x".to_string()),
                "proxy_exec_failed",
                500,
            ),
        ];
        for (failure, code, status) in cases {
            assert_eq!(failure.code(), code);
            assert_eq!(failure.status(), status);
            assert!(!failure.to_string().is_empty());
        }
    }

    #[test]
    fn slot_device_names_follow_kernel_virtio_naming() {
        assert_eq!(proxy_slot_device_name(0), "vdc");
        assert_eq!(proxy_slot_device_name(23), "vdz");
        assert_eq!(proxy_slot_device_name(24), "vdaa");
        assert_eq!(proxy_slot_device_name(25), "vdab");
        assert_eq!(proxy_slot_device_name(49), "vdaz");
        assert_eq!(proxy_slot_device_name(50), "vdba");
        assert_eq!(proxy_slot_device_name(699), "vdzz");
    }
}

//! Resident proxy sandbox startup (proxy-sandbox design §5.1).
//!
//! The proxy sandbox is a long-lived VM booted with N deferred slot drives:
//! block devices attached without a boot-time guest mount (`defer_mount`).
//! Every slot device is backed read-only by the same overlaybd stack as the
//! proxy's own rootfs image, so all slot devices share one set of local layer
//! files. Slot devices are later rebound to paused sandboxes' rootfs stacks
//! and mounted on demand by envd (BIND flow, implemented separately).
//!
//! Startup prefers restoring from the proxy template (a committed snapshot of
//! a freshly booted proxy, <50ms) and falls back to a cold start; after a
//! cold start completes, the running proxy is captured and published as the
//! template for subsequent boots. The template alias embeds a fingerprint of
//! the proxy shape (image, slot count, runtime versions) so configuration
//! changes transparently rebuild the template.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::cfg::{ConfigManager, ProxyConfig};
use crate::image::{ImageResolver, ResolvedBlockImage};
use crate::orchestrator::service::configured_runtime_versions;
use crate::orchestrator::types::{CreateSandboxRequest, SandboxLaunchSource};
use crate::orchestrator::{Orchestrator, OrchestratorError, SandboxMetadata, SandboxTimeoutAction};
use crate::sandbox::{ExtraDrive, SandboxNetworkPolicy};
use crate::snapshot::{
    CommandContext, RepositoryError, RunnableSnapshot, SnapshotAlias, SnapshotId, SnapshotManager,
    SnapshotPublishMetadata, SnapshotPublishSource,
};
use crate::types::{ImageConfigs, SandboxResources};

/// Proxy sandboxes are node infrastructure: use a very long TTL so the
/// auto-pause/eviction tasks never fire in practice. A first-class
/// no-expiry/system-sandbox marker replaces this once defined.
const PROXY_SANDBOX_TTL: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

/// `user_metadata` key marking a sandbox as the resident proxy. The value
/// carries the reserved slot count.
pub const PROXY_METADATA_KEY: &str = "aenv:proxy";

/// Deferred slot drive id prefix; slot `i` is `proxy_slot_{i}` and maps to
/// guest device `vd(c + i)` under the kernel's virtio-blk naming (multi-letter
/// beyond `vdz`).
const SLOT_DRIVE_ID_PREFIX: &str = "proxy_slot_";

/// Guest directory under which a bound target's rootfs is mounted
/// (`{root}/{target-sandbox-id}`). Commands are chrooted here.
pub const PROXY_TARGET_MOUNT_ROOT: &str = "/mnt/aenv-proxy-targets";

/// Deferred slot drive id for slot `index`.
pub fn proxy_slot_drive_id(index: usize) -> String {
    format!("{SLOT_DRIVE_ID_PREFIX}{index}")
}

/// Guest device name for slot `index`, matching the kernel's virtio-blk
/// auto-naming (`virtblk_name_format` in drivers/block/virtio_blk.c):
/// `vdc..vdz` for the first 24 slots, then `vdaa..vdzz`, then `vdaaa..`.
///
/// vda is the tools drive and vdb the proxy rootfs, so slots start at vdc
/// (kernel disk index = slot + 2). envd mounts whatever path it is given and
/// never parses the letter, so multi-letter names need no guest-side
/// cooperation.
pub fn proxy_slot_device_name(index: usize) -> String {
    // Bijective base-26 encoding of the kernel's disk index, identical to
    // virtblk_name_format: emit `index % 26`, then iterate on
    // `index / 26 - 1` until negative.
    const BASE: usize = 26;
    let mut disk_index = index + 2;
    let mut letters: Vec<u8> = Vec::with_capacity(4);
    loop {
        letters.push(b'a' + (disk_index % BASE) as u8);
        disk_index = disk_index / BASE;
        if disk_index == 0 {
            break;
        }
        disk_index -= 1;
    }
    letters.reverse();
    let suffix: String = letters.iter().map(|&b| b as char).collect();
    format!("vd{suffix}")
}

/// Prefix of the committed-snapshot alias under which the proxy template is
/// published and looked up.
const PROXY_TEMPLATE_ALIAS_PREFIX: &str = "aenv-proxy";

/// Whether `metadata` marks a sandbox as the resident proxy.
pub fn is_proxy_sandbox(metadata: &SandboxMetadata) -> bool {
    metadata
        .user_metadata
        .as_ref()
        .is_some_and(|meta| meta.contains_key(PROXY_METADATA_KEY))
}

/// Start the resident proxy sandbox pool at server startup.
///
/// Prefers restoring from the proxy template; falls back to a cold start and
/// publishes the running proxy as the new template. Stale proxy sandboxes
/// from previous runs are swept first so restarts never accumulate proxy
/// records.
///
/// Test-mode policy: every unexpected failure (unreadable template, slot
/// registration failure, template publish failure) is fatal — the server
/// terminates with a loud log instead of silently serving without a proxy.
pub async fn init_proxy_pool(
    orchestrator: Arc<Orchestrator>,
    image_resolver: Arc<ImageResolver>,
    snapshot_manager: Arc<SnapshotManager>,
) -> Result<()> {
    use super::proxy_binding::{proxy_fatal, ProxySpawner};

    let proxy_config = ConfigManager::global_config().proxy.clone();
    if !proxy_config.enabled {
        return Ok(());
    }

    sweep_stale_proxy_sandboxes(&orchestrator).await;

    // Install the on-demand spawner first so pause hooks arriving while the
    // first proxy cold-starts can scale the pool out once it is ready.
    let spawner: Arc<dyn ProxySpawner> = Arc::new(ProxyPoolSpawner {
        orchestrator: Arc::clone(&orchestrator),
        snapshot_manager: Arc::clone(&snapshot_manager),
    });
    orchestrator.proxy_bindings().begin_pool_init(spawner).await;

    info!(
        image = %proxy_config.image,
        slots = proxy_config.slots_per_proxy,
        "starting resident proxy sandbox"
    );

    let alias = proxy_template_alias(&proxy_config)?;

    // Preferred path: restore from the template committed by a previous run.
    // A *missing* template (first ever boot) legitimately cold-starts; an
    // *unreadable* one is fatal in test mode.
    match snapshot_manager.load_runnable(alias.as_ref()).await {
        Ok(Some(snapshot)) => {
            let metadata = create_proxy_from_template(&orchestrator, snapshot, &proxy_config)
                .await
                .unwrap_or_else(|err| {
                    proxy_fatal(
                        "failed to start proxy sandbox from template",
                        format_args!("alias={alias}: {err:#}"),
                    )
                });
            let slots = orchestrator
                .register_proxy_sandbox(metadata.id)
                .await
                .unwrap_or_else(|err| {
                    proxy_fatal(
                        "proxy sandbox slots could not be registered",
                        format_args!("sandbox {}: {err:#}", metadata.id),
                    )
                });
            if slots == 0 {
                proxy_fatal(
                    "proxy sandbox registered zero slots",
                    format_args!(
                        "sandbox {} started from template {} but exposes no deferred slot drives",
                        metadata.id, alias
                    ),
                );
            }
            info!(
                sandbox_id = %metadata.id,
                alias = %alias,
                slots,
                "resident proxy sandbox started from template"
            );
            orchestrator.proxy_bindings().mark_ready();
            return Ok(());
        }
        Ok(None) => {}
        Err(err) => {
            proxy_fatal(
                "failed to load proxy template",
                format_args!("alias={alias}: {err:#}"),
            );
        }
    }

    // First-boot path: cold start, then capture the running proxy as the
    // template for subsequent boots.
    let request = build_proxy_launch_request(&proxy_config, image_resolver.as_ref()).await?;
    let metadata = orchestrator
        .create_sandbox(request)
        .await
        .context("create proxy sandbox")?;
    let slots = orchestrator
        .register_proxy_sandbox(metadata.id)
        .await
        .unwrap_or_else(|err| {
            proxy_fatal(
                "proxy sandbox slots could not be registered",
                format_args!("sandbox {}: {err:#}", metadata.id),
            )
        });
    if slots == 0 {
        proxy_fatal(
            "proxy sandbox registered zero slots",
            format_args!(
                "sandbox {} cold-started but exposes no deferred slot drives",
                metadata.id
            ),
        );
    }
    info!(
        sandbox_id = %metadata.id,
        slots,
        "resident proxy sandbox started via cold start"
    );
    orchestrator.proxy_bindings().mark_ready();

    publish_proxy_template(&orchestrator, &snapshot_manager, metadata.id, alias.clone())
        .await
        .unwrap_or_else(|err| {
            proxy_fatal(
                "failed to publish proxy template",
                format_args!("sandbox {} alias={alias}: {err:#}", metadata.id),
            )
        });
    Ok(())
}

/// On-demand proxy spawner: creates one more resident proxy sandbox from the
/// template and registers its slots.
struct ProxyPoolSpawner {
    orchestrator: Arc<Orchestrator>,
    snapshot_manager: Arc<SnapshotManager>,
}

#[async_trait::async_trait]
impl super::proxy_binding::ProxySpawner for ProxyPoolSpawner {
    async fn spawn_proxy(&self) -> Result<crate::types::SandboxId> {
        use super::proxy_binding::proxy_fatal;

        let proxy_config = ConfigManager::global_config().proxy.clone();
        let alias = proxy_template_alias(&proxy_config)?;

        let metadata = match self.snapshot_manager.load_runnable(alias.as_ref()).await {
            Ok(Some(snapshot)) => {
                create_proxy_from_template(&self.orchestrator, snapshot, &proxy_config)
                    .await
                    .unwrap_or_else(|err| {
                        proxy_fatal(
                            "on-demand proxy: failed to start from template",
                            format_args!("alias={alias}: {err:#}"),
                        )
                    })
            }
            Ok(None) => {
                // The template is published right after the first cold start;
                // reaching here means publish failed or races init — fatal.
                proxy_fatal(
                    "on-demand proxy: no template to spawn from",
                    format_args!(
                        "proxy template {alias} is missing; init_proxy_pool should have published it"
                    ),
                );
            }
            Err(err) => {
                proxy_fatal(
                    "on-demand proxy: failed to load template",
                    format_args!("alias={alias}: {err:#}"),
                );
            }
        };

        let slots = self
            .orchestrator
            .register_proxy_sandbox(metadata.id)
            .await
            .unwrap_or_else(|err| {
                proxy_fatal(
                    "on-demand proxy: slots could not be registered",
                    format_args!("sandbox {}: {err:#}", metadata.id),
                )
            });
        if slots == 0 {
            proxy_fatal(
                "on-demand proxy registered zero slots",
                format_args!("sandbox {} exposes no deferred slot drives", metadata.id),
            );
        }
        info!(
            sandbox_id = %metadata.id,
            slots,
            "additional resident proxy sandbox spawned and registered"
        );
        Ok(metadata.id)
    }
}

/// Delete every sandbox marked as a proxy. The proxy is disposable
/// infrastructure: it is restored from its template (or cold-started when
/// the template is missing) on every server start, so lingering records
/// only waste disk space.
///
/// Test-mode policy: a failed sweep leaves stale proxies that would hold
/// slots and shadow the freshly registered pool, so it is fatal.
async fn sweep_stale_proxy_sandboxes(orchestrator: &Arc<Orchestrator>) {
    use super::proxy_binding::proxy_fatal;

    let stale = match orchestrator.list_sandboxes().await {
        Ok(list) => list
            .into_iter()
            .filter(is_proxy_sandbox)
            .map(|metadata| metadata.id)
            .collect::<Vec<_>>(),
        Err(err) => proxy_fatal(
            "failed to list sandboxes while sweeping stale proxy sandboxes",
            format_args!("{err:#}"),
        ),
    };
    for sandbox_id in stale {
        // A proxy has no volume mounts, so deletion goes straight through
        // without the pre-delete volume snapshot.
        match orchestrator.delete_sandbox(sandbox_id).await {
            Ok(()) => info!(sandbox_id = %sandbox_id, "swept stale proxy sandbox"),
            Err(OrchestratorError::SandboxNotFound(_)) => {}
            Err(err) => proxy_fatal(
                "failed to sweep stale proxy sandbox",
                format_args!("sandbox {sandbox_id}: {err:#}"),
            ),
        }
    }
}

/// Capture the running proxy sandbox and publish it as the proxy template.
///
/// `capture_snapshot` drives Running -> Snapshotting -> Running: the proxy
/// stays up and keeps serving after the template exists. Failures are
/// logged (the proxy itself is unaffected); the next server start retries
/// the cold-start + publish path.
async fn publish_proxy_template(
    orchestrator: &Arc<Orchestrator>,
    snapshot_manager: &Arc<SnapshotManager>,
    sandbox_id: crate::types::SandboxId,
    alias: SnapshotAlias,
) -> Result<()> {
    // TODO(proxy): drop guest caches before capture so the template memory
    // stays lean (needs a server-internal exec path into the sandbox).
    let capture = orchestrator
        .capture_snapshot(sandbox_id)
        .await
        .context("capture proxy template snapshot")?;

    let publish_result = snapshot_manager
        .publish_captured(
            SnapshotPublishMetadata {
                id: SnapshotId::generate(),
                alias: Some(alias.clone()),
                source: SnapshotPublishSource::Sandbox {
                    source_sandbox_id: sandbox_id.to_string(),
                },
                context: capture.metadata.context.clone(),
                startup: capture.metadata.startup.clone(),
                resources: capture.metadata.resources,
                runtime_versions: capture.metadata.runtime_versions.clone(),
                virtualization_mode: capture.metadata.virtualization_mode,
                image_configs: capture.metadata.image_configs.clone(),
                // The proxy never mounts volumes, so its template carries none.
                volume_snapshots: Vec::new(),
                custom_extension_params: capture.metadata.custom_extension_params.clone(),
            },
            capture.captured_snapshot,
        )
        .await;

    match publish_result {
        Ok(record) => {
            info!(
                snapshot_id = %record.id,
                alias = %alias,
                "proxy template published"
            );
            Ok(())
        }
        // The cold-start publish runs only when the alias was free, so a
        // conflict here means something else bound it concurrently — a
        // silent "keep the other template" would mask that in test mode.
        Err(RepositoryError::AliasConflict { .. }) => {
            super::proxy_binding::proxy_fatal(
                "proxy template alias conflict during publish",
                format_args!("alias {alias} was bound by another record while publishing"),
            );
        }
        Err(err) => Err(anyhow::Error::new(err)).context("publish proxy template"),
    }
}

/// Build the stable alias identifying the proxy template for the current
/// proxy shape.
///
/// The fingerprint covers the base image, slot count, and every runtime
/// version that a snapshot restore must stay compatible with (kernel,
/// firecracker, envd, tools drive). Any change produces a different alias,
/// so the next boot cold-starts a fresh proxy and publishes it as the new
/// template instead of restoring a stale one.
fn proxy_template_alias(proxy_config: &ProxyConfig) -> Result<SnapshotAlias> {
    let runtime = configured_runtime_versions();
    let fingerprint = format!(
        "{}|{}|{}|{}|{}|{}",
        proxy_config.image,
        proxy_config.slots_per_proxy,
        runtime.kernel_version,
        runtime.firecracker_version,
        runtime.envd_version,
        runtime.tools_drive_version,
    );
    // Truncate the fingerprint to 48 bits / 12 hex chars for a readable alias.
    let fingerprint_hash = fnv1a64(fingerprint.as_bytes()) & 0xffff_ffff_ffff;
    let alias = format!(
        "{PROXY_TEMPLATE_ALIAS_PREFIX}-s{}-{fingerprint_hash:012x}",
        proxy_config.slots_per_proxy,
    );
    SnapshotAlias::parse(&alias).context("build proxy template alias")
}

/// FNV-1a 64-bit: small, dependency-free, and stable across compiler and
/// process restarts (unlike `DefaultHasher`, whose algorithm is unspecified).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn proxy_user_metadata(slots: usize) -> HashMap<String, String> {
    let mut metadata = HashMap::new();
    metadata.insert(PROXY_METADATA_KEY.to_string(), slots.to_string());
    metadata
}

/// Create the resident proxy by restoring the proxy template snapshot.
///
/// The template already carries the deferred slot drives (its device
/// topology was frozen at capture time); no launch-time drives are added.
async fn create_proxy_from_template(
    orchestrator: &Arc<Orchestrator>,
    snapshot: RunnableSnapshot,
    proxy_config: &ProxyConfig,
) -> Result<SandboxMetadata> {
    let request = CreateSandboxRequest {
        source: SandboxLaunchSource::Snapshot(Box::new(snapshot)),
        extra_drives: Vec::new(),
        // The template carries no volume snapshots and no launch-time drives
        // are supplied, matching the plain template-restore request shape.
        extra_drives_in_snapshot: false,
        timeout: Some(PROXY_SANDBOX_TTL),
        timeout_action: SandboxTimeoutAction::Pause,
        auto_resume: false,
        user_metadata: Some(proxy_user_metadata(proxy_config.slots_per_proxy)),
        env_vars: None,
        network_policy: SandboxNetworkPolicy::default(),
        secure: true,
        custom_extension_params: None,
        volume_mounts: HashMap::new(),
    };
    orchestrator
        .create_sandbox(request)
        .await
        .context("create proxy sandbox from template")
}

/// Translate the `[proxy]` config into a standard sandbox creation request.
///
/// This is the single source of truth for the "proxy shape": the rootfs is
/// the configured base image, and every slot drive references the same
/// resolved image config read-only with `defer_mount`, attached in index
/// order so the slot-to-device-letter mapping stays deterministic.
async fn build_proxy_launch_request(
    proxy_config: &ProxyConfig,
    image_resolver: &ImageResolver,
) -> Result<CreateSandboxRequest> {
    let ResolvedBlockImage {
        image_ref,
        overlaybd_config_path,
        base_context,
        raw_config,
    } = image_resolver
        .resolve(&proxy_config.image)
        .await
        .with_context(|| format!("resolve proxy image '{}'", proxy_config.image))?;

    // Slot placeholder drives: one deferred extra drive per slot, all sharing
    // the rootfs image config so the ublk daemon opens one stack's files for
    // every device (shared page cache, per-device index contexts only).
    let mut slots = Vec::with_capacity(proxy_config.slots_per_proxy);
    for index in 0..proxy_config.slots_per_proxy {
        let drive = ExtraDrive::try_new_overlaybd(
            proxy_slot_drive_id(index),
            overlaybd_config_path.clone(),
            true,
        )
        .context("build proxy slot drive")?
        .with_defer_mount(true);
        slots.push(drive);
    }

    let mut image_configs = ImageConfigs::new();
    if let Some(config) = &raw_config {
        image_configs.add(None::<String>, "/", config.clone());
    }

    let machine = &ConfigManager::global_config().machine;
    let resources = SandboxResources {
        cpu_count: machine.vcpu_count,
        memory_mib: machine.mem_size_mib,
        // Zero means omitted; the orchestrator fills it from the resolved rootfs.
        disk_size_mib: 0,
    };

    let context = CommandContext::from_env_and_workdir(base_context.env_vars, base_context.workdir)
        .with_user(base_context.user)
        .with_exposed_ports(base_context.exposed_ports)
        .with_entrypoint(base_context.entrypoint)
        .with_cmd(base_context.cmd)
        .with_volumes(base_context.volumes)
        .with_labels(base_context.labels);

    Ok(CreateSandboxRequest {
        source: SandboxLaunchSource::Image {
            image_ref,
            overlaybd_config_path,
            context: Box::new(context),
            resources: Some(resources),
            extra_drives: slots,
            extra_boot_args: None,
            image_configs: Box::new(image_configs),
        },
        extra_drives: Vec::new(),
        extra_drives_in_snapshot: false,
        timeout: Some(PROXY_SANDBOX_TTL),
        timeout_action: SandboxTimeoutAction::Pause,
        auto_resume: false,
        user_metadata: Some(proxy_user_metadata(proxy_config.slots_per_proxy)),
        env_vars: None,
        network_policy: SandboxNetworkPolicy::default(),
        secure: true,
        custom_extension_params: None,
        volume_mounts: HashMap::new(),
    })
}

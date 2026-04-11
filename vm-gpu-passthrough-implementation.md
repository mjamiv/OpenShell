# VM GPU passthrough: implementation plan

> Design: [vm-gpu-passthrough.md](vm-gpu-passthrough.md)

## Phase 0 -- Specification and failing test (current)

- [x] Design doc.
- [x] Phase 0.5 VMM decision (cloud-hypervisor selected).
- [ ] **`gpu_passthrough` module** integrated into `crates/openshell-vm/src/`:
  - `probe_host_nvidia_vfio_readiness()` -- Linux sysfs scan; non-Linux returns `UnsupportedPlatform`.
  - `nvidia_gpu_available_for_vm_passthrough()` -- hard-coded `false` until end-to-end passthrough works.
  - **Note:** `gpu_passthrough.rs` and `gpu_passthrough_implementation.rs` exist as untracked files at the repo root but are not wired into the crate module tree (`lib.rs` does not `mod gpu_passthrough;`). Move them into `crates/openshell-vm/src/`, add `pub mod gpu_passthrough;`, and ensure `cargo test -p openshell-vm` compiles them.
- [ ] **Failing integration test** `tests/gpu_passthrough_implementation.rs` -- documents the target and fails until implementation is finished.

**Running the red test:** `cargo test -p openshell-vm --test gpu_passthrough_implementation`

**Note:** `mise run test` uses `cargo test --workspace --exclude openshell-vm`, so default CI stays green.

---

## Phase 1 -- VMM backend abstraction and cloud-hypervisor integration

### 1a. Backend trait and libkrun extraction

Refactor only -- no behavior changes. Existing tests must still pass.

- [ ] Create `src/backend.rs` with the `VmBackend` trait:

```rust
pub trait VmBackend {
    fn launch(&self, config: &VmLaunchConfig) -> Result<i32, VmError>;
}

pub struct VmLaunchConfig {
    pub base: VmConfig,
    pub vfio_device: Option<String>,
}
```

- [ ] Create `src/backend/libkrun.rs` -- move into `LibkrunBackend`:
  - `VmContext` struct and all methods (current `lib.rs` lines 584-811)
  - gvproxy setup block inside `NetBackend::Gvproxy` (lines 1337-1466)
  - fork + waitpid + signal forwarding (lines 1525-1710)
  - bootstrap block (lines 1648-1663)
- [ ] Extract shared gvproxy startup into a helper used by both backends.
- [ ] Update `launch()` to dispatch:

```rust
pub fn launch(config: &VmLaunchConfig) -> Result<i32, VmError> {
    // ... existing pre-launch checks ...

    if config.vfio_device.is_some() {
        #[cfg(not(target_os = "linux"))]
        return Err(VmError::HostSetup(
            "GPU passthrough requires Linux with KVM and IOMMU".into(),
        ));

        #[cfg(target_os = "linux")]
        {
            let backend = CloudHypervisorBackend::new()?;
            return backend.launch(config);
        }
    }

    LibkrunBackend.launch(config)
}
```

- [ ] `ffi.rs` stays as-is -- only used by `LibkrunBackend`.

### 1b. cloud-hypervisor backend

- [ ] Create `src/backend/cloud_hypervisor.rs` implementing `VmBackend`.
- [ ] REST API client -- HTTP/1.1 over Unix socket, ~5 endpoints:

```
PUT /api/v1/vm.create   -- configure VM
PUT /api/v1/vm.boot     -- start VM
PUT /api/v1/vm.shutdown -- graceful stop
GET /api/v1/vm.info     -- status check
PUT /api/v1/vm.delete   -- cleanup
```

Use `hyper` over Unix socket (already in dependency tree) or raw HTTP. Avoid adding `cloud-hypervisor-client` crate for ~5 calls.

- [ ] VM create payload mapping from `VmConfig`:

```json
{
  "cpus": { "boot_vcpus": 4 },
  "memory": { "size": 8589934592 },
  "payload": {
    "kernel": "/path/to/vmlinux",
    "cmdline": "console=hvc0 root=virtiofs:rootfs rw init=/srv/openshell-vm-init.sh"
  },
  "fs": [
    { "tag": "rootfs", "socket": "/path/to/virtiofsd.sock", "num_queues": 1, "queue_size": 1024 }
  ],
  "disks": [
    { "path": "/path/to/state.raw", "readonly": false }
  ],
  "net": [
    { "socket": "/path/to/gvproxy-qemu.sock", "mac": "5a:94:ef:e4:0c:ee" }
  ],
  "vsock": {
    "cid": 3,
    "socket": "/path/to/vsock.sock"
  },
  "devices": [
    { "path": "/sys/bus/pci/devices/0000:41:00.0/" }
  ],
  "serial": { "mode": "File", "file": "/path/to/console.log" },
  "console": { "mode": "Off" }
}
```

- [ ] Process lifecycle:
  1. Start `cloud-hypervisor --api-socket /tmp/ovm-chv-{id}.sock` as subprocess
  2. Wait for API socket to appear (exponential backoff, same pattern as gvproxy)
  3. `PUT vm.create` with config payload
  4. `PUT vm.boot`
  5. Parent waits on subprocess
  6. Signal forwarding: SIGINT/SIGTERM -> `PUT vm.shutdown` + subprocess SIGTERM
  7. Cleanup: remove API socket

### 1c. Kernel extraction and build pipeline

- [ ] Modify `build-libkrun.sh`: after building libkrunfw, copy `vmlinux` from the kernel build tree to `target/libkrun-build/vmlinux` before cleanup.
- [ ] Add to `openshell.kconfig` (harmless for non-GPU boots):

```
CONFIG_PCI=y
CONFIG_PCI_MSI=y
CONFIG_DRM=y
CONFIG_MODULES=y
CONFIG_MODULE_UNLOAD=y
```

- [ ] Add to `pins.env`:

```bash
CLOUD_HYPERVISOR_VERSION="${CLOUD_HYPERVISOR_VERSION:-v42.0}"
VIRTIOFSD_VERSION="${VIRTIOFSD_VERSION:-v1.13.0}"
```

- [ ] Create `build-cloud-hypervisor.sh` (or download step): download pre-built static binary from cloud-hypervisor GitHub releases for the target architecture.
- [ ] Update `package-vm-runtime.sh`: include `cloud-hypervisor`, `vmlinux`, and `virtiofsd` in the runtime tarball for Linux builds.
- [ ] `validate_runtime_dir()` in `lib.rs` must **not** require GPU binaries. Only `CloudHypervisorBackend::new()` validates their presence.

### 1d. vsock exec agent compatibility

libkrun uses per-port vsock bridging (`krun_add_vsock_port2`): each guest vsock port maps to a host Unix socket. cloud-hypervisor uses standard vhost-vsock with a single socket and CID-based addressing.

- [ ] Update `exec.rs` to support both connection modes:
  - **libkrun**: connect to `vm_exec_socket_path()` (existing)
  - **cloud-hypervisor**: connect via `AF_VSOCK` (CID 3, port 10777) or bridge with `socat`
- [ ] Test exec agent communication (cat, env) over both backends.

### 1e. Plumb `--gpu` flag

- [ ] Add fields to `VmConfig`:

```rust
pub vfio_device: Option<String>,
pub gpu_enabled: bool,
```

- [ ] When `gpu_enabled` is set, add `GPU_ENABLED=true` to guest environment.
- [ ] Wire `--gpu` / `--gpu <pci-addr>` from the CLI to `VmConfig`.

---

## Phase 1.5 -- Guest rootfs: NVIDIA driver and toolkit

- [ ] **NVIDIA driver in rootfs.** Options:
  - **Separate GPU rootfs artifact**: build `rootfs-gpu.tar.zst` alongside `rootfs.tar.zst`. Launcher selects GPU variant when `--gpu` is passed.
  - **Bake into rootfs**: use `nvcr.io/nvidia/base/ubuntu` base image from `pins.env`. Heavier (~2-3 GB) but self-contained.
  - **Runtime injection via virtio-fs**: stage driver packages on host, mount into guest. Lighter but more complex.
- [ ] **Driver version compatibility**: document minimum driver version and GPU compute capability.
- [ ] **NVIDIA container toolkit**: install `nvidia-container-toolkit` so `nvidia-container-runtime` is available to containerd/k3s.
- [ ] **Smoke test**: `nvidia-smi` runs inside the guest after rootfs build.

---

## Phase 2 -- Guest appliance parity

- [ ] **Init script changes** (`openshell-vm-init.sh`): when `GPU_ENABLED=true`:
  - Load NVIDIA kernel modules (`nvidia`, `nvidia_uvm`, `nvidia_modeset`)
  - Run `nvidia-smi` -- fail fast if device not visible
  - Copy `gpu-manifests/*.yaml` into k3s auto-deploy directory (mirrors `cluster-entrypoint.sh` ~line 384)
  - Verify `nvidia-container-runtime` is registered with containerd
- [ ] **End-to-end validation**: sandbox pod requesting `nvidia.com/gpu: 1` gets scheduled and can run `nvidia-smi` inside the pod.

---

## Phase 3 -- CLI / UX

- [ ] Mirror `openshell gateway start --gpu` semantics for VM backend.
- [ ] Support `--gpu <pci-addr>` for multi-GPU hosts.
- [ ] Document host preparation (IOMMU, `vfio-pci`, unbinding `nvidia`).
- [ ] Document single-GPU caveats (host display loss, headless operation).

---

## Phase 4 -- CI

- [ ] GPU E2E job: optional runner with `OPENSHELL_VM_GPU_E2E=1` and a VFIO-bound GPU. Tighten `nvidia_gpu_available_for_vm_passthrough()` to require `VfioBoundReady` + guest smoke.
- [ ] Non-GPU cloud-hypervisor CI test: boot and exec agent check without VFIO. Catches backend regressions without GPU hardware.

---

## Test evolution

Today `nvidia_gpu_available_for_vm_passthrough()` returns `false`. When complete, it should compose:

1. `probe_host_nvidia_vfio_readiness()` returns `VfioBoundReady` (clean IOMMU group)
2. cloud-hypervisor binary present in runtime bundle
3. `/dev/vfio/vfio` and `/dev/vfio/{group}` accessible
4. Guest rootfs includes NVIDIA driver and toolkit

Options for the final gate:
- `true` only when CI env var is set and hardware verified
- Replace boolean with full integration check
- Remove `#[ignore]` and run only on GPU runners

Pick one in the final PR so `mise run test` policy stays intentional.

---

## File change index

| File | Change |
|---|---|
| `crates/openshell-vm/src/lib.rs` | Extract `launch()` internals into backend dispatch; add `vfio_device` / `gpu_enabled` to `VmConfig` |
| `crates/openshell-vm/src/backend.rs` (new) | `VmBackend` trait, `VmLaunchConfig` |
| `crates/openshell-vm/src/backend/libkrun.rs` (new) | `LibkrunBackend` -- moved from `lib.rs` (mechanical refactor) |
| `crates/openshell-vm/src/backend/cloud_hypervisor.rs` (new) | `CloudHypervisorBackend` -- REST API client, process lifecycle, VFIO assignment |
| `crates/openshell-vm/src/ffi.rs` | No changes (used only by `LibkrunBackend`) |
| `crates/openshell-vm/src/exec.rs` | Support both libkrun Unix socket and vhost-vsock connection modes |
| `crates/openshell-vm/src/gpu_passthrough.rs` (move from repo root) | `probe_host_nvidia_vfio_readiness()` with IOMMU group check |
| `crates/openshell-vm/runtime/kernel/openshell.kconfig` | Add `CONFIG_PCI`, `CONFIG_PCI_MSI`, `CONFIG_DRM`, `CONFIG_MODULES`, `CONFIG_MODULE_UNLOAD` |
| `crates/openshell-vm/pins.env` | Add `CLOUD_HYPERVISOR_VERSION`, `VIRTIOFSD_VERSION` |
| `crates/openshell-vm/scripts/openshell-vm-init.sh` | GPU-gated block: module loading, `nvidia-smi` check, manifest copy |
| `tasks/scripts/vm/build-libkrun.sh` | Preserve `vmlinux` in `target/libkrun-build/` |
| `tasks/scripts/vm/build-cloud-hypervisor.sh` (new) | Download or build cloud-hypervisor static binary |
| `tasks/scripts/vm/package-vm-runtime.sh` | Include `cloud-hypervisor`, `vmlinux`, `virtiofsd` for Linux builds |

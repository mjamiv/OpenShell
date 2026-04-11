# VM GPU passthrough: design

> Status: **Design complete.** Implementation tracked in [vm-gpu-passthrough-implementation.md](vm-gpu-passthrough-implementation.md).

## Goal

Match the **Docker cluster GPU path** (`openshell gateway start --gpu`): the k3s node inside the microVM sees a **real NVIDIA GPU** so sandbox pods can request `nvidia.com/gpu`, the NVIDIA device plugin, and `nvidia` RuntimeClass behave identically to the Docker path.

This is **PCI passthrough** (VFIO) of the physical GPU into the guest -- not virtio-gpu / Venus / virgl.

## Decision record

### Venus / virgl rejected

libkrun's virtio-gpu Venus path forwards **Vulkan** API calls, not NVIDIA's proprietary CUDA stack. The guest never loads the NVIDIA kernel driver and has no `/dev/nvidia*` device nodes. This rules out `nvidia-smi`, CUDA workloads, the k8s device plugin, and the NVIDIA container runtime -- all of which the Docker `--gpu` path depends on.

| Requirement | Venus | VFIO passthrough |
|---|---|---|
| `nvidia-smi` in guest | No (Vulkan only) | Yes (bare-metal driver) |
| CUDA workloads | No | Yes |
| `nvidia.com/gpu` k8s resource | No | Yes |
| NVIDIA container runtime | No | Yes |
| Performance | ~75-80% (forwarding overhead) | ~100% (bare-metal) |
| macOS support | Yes (MoltenVK) | No (Linux IOMMU only) |

### libkrun VFIO rejected

libkrun upstream closed the device passthrough request ([containers/libkrun#32](https://github.com/containers/libkrun/issues/32), March 2023). VFIO would require PCI bus emulation and ACPI tables -- outside libkrun's MMIO-only virtio design. No known forks add this.

### VMM selection: dual backend

| VMM | VFIO | Size | vsock | Rust | macOS | Decision |
|-----|------|------|-------|------|-------|----------|
| **libkrun** v1.17.4 | No | ~5 MB | Yes | Yes | Yes (HVF) | Keep for non-GPU |
| **cloud-hypervisor** | Yes | ~10 MB | Yes | Yes | No | **GPU backend** |
| QEMU | Yes | ~50+ MB | Yes | No (C) | Limited | Rejected: size, C |
| crosvm | Yes | ~15 MB | Yes | Yes | No | Rejected: heavier |
| libkrun fork | Needs patches | ~5 MB | Yes | Yes | Possible | Rejected: maintenance |

**cloud-hypervisor** is the GPU-only VMM backend. libkrun remains the default for all non-GPU workloads and is the only backend on macOS.

---

## Architecture

```
                        openshell gateway start
                                 |
                         ┌───────┴───────┐
                         │  --gpu flag?   │
                         └───────┬───────┘
                        no /           \ yes (Linux only)
                          /             \
                  ┌──────┴──────┐  ┌────┴─────────────┐
                  │ LibkrunBack │  │ CloudHvBackend    │
                  │   end       │  │                   │
                  │             │  │ REST API over     │
                  │ ffi.rs      │  │ Unix socket       │
                  │ (dlopen)    │  │                   │
                  └──────┬──────┘  └────┬─────────────┘
                         │              │
                  ┌──────┴──────┐  ┌────┴─────────────┐
                  │ libkrun VM  │  │ cloud-hypervisor  │
                  │             │  │ VM                │
                  │ virtio-fs   │  │ virtio-fs         │
                  │ virtio-net  │  │ virtio-net        │
                  │ vsock       │  │ vsock             │
                  │ virtio-blk  │  │ virtio-blk        │
                  │             │  │ VFIO PCI (GPU)    │
                  └─────────────┘  └──────────────────┘
```

### Shared across both backends

- **Guest rootfs**: Same directory tree under `~/.local/share/openshell/openshell-vm/{version}/instances/<name>/rootfs/`.
- **Init script**: `/srv/openshell-vm-init.sh` runs as PID 1. GPU behavior is gated on `GPU_ENABLED=true`.
- **Exec agent**: `openshell-vm-exec-agent.py` on vsock port 10777.
- **gvproxy**: DNS, DHCP, and port forwarding. Both backends connect to gvproxy's QEMU-mode Unix socket.
- **Host bootstrap**: `bootstrap_gateway()` fetches PKI over the exec agent and stores mTLS creds.

### Per-backend differences

| Concern | libkrun | cloud-hypervisor |
|---|---|---|
| **Process model** | Library via `dlopen`; `fork()` + `krun_start_enter()` | Subprocess; REST API over Unix socket |
| **Boot model** | `krun_set_root(dir)` + `krun_set_exec(init)` -- kernel in libkrunfw | `--kernel vmlinux` + virtio-fs via virtiofsd -- explicit kernel binary |
| **Networking** | `krun_add_net_unixstream` (Linux) / `krun_add_net_unixgram` (macOS) | `--net socket=/path/to/gvproxy.sock` |
| **vsock** | `krun_add_vsock_port2(port, socket)` per port | `--vsock cid=3,socket=/path/to/vsock.sock` (vhost-vsock) |
| **Block storage** | `krun_add_disk3(id, path, format, ...)` | `--disk path=/path/to/state.raw` |
| **GPU** | N/A | `--device path=/sys/bus/pci/devices/ADDR/` (VFIO) |
| **Console** | `krun_set_console_output(path)` | `--serial file=/path` |
| **Lifecycle** | `krun_free_ctx` in `Drop`; `waitpid` on child | REST: `vm.create` -> `vm.boot` -> `vm.shutdown`; wait on subprocess |
| **macOS** | Yes (HVF) | No (KVM only) |

---

## Host requirements (GPU path)

### Host kernel

- `CONFIG_VFIO`, `CONFIG_VFIO_PCI`, `CONFIG_VFIO_IOMMU_TYPE1`
- IOMMU enabled: BIOS (VT-d / AMD-Vi) + kernel params (`intel_iommu=on iommu=pt` or AMD equivalent)

### Host preparation

1. Unbind GPU from `nvidia` driver: `echo <pci-addr> > /sys/bus/pci/drivers/nvidia/unbind`
2. Bind to `vfio-pci`: `echo <vendor-id> <device-id> > /sys/bus/pci/drivers/vfio-pci/new_id`
3. Verify: `readlink /sys/bus/pci/devices/<pci-addr>/driver` points to `vfio-pci`
4. Ensure `/dev/vfio/vfio` and `/dev/vfio/{group}` are accessible

### Host preflight state machine

The stack classifies each NVIDIA PCI device into one of these states:

| State | Meaning | Action |
|---|---|---|
| `NoNvidiaDevice` | No NVIDIA PCI device found | Error: no GPU to pass through |
| `BoundToNvidia` | Device on `nvidia` driver | Not available until unbound and rebound to `vfio-pci` |
| `VfioBoundDirtyGroup` | On `vfio-pci` but IOMMU group has non-VFIO peers | Report which peers need unbinding |
| `VfioBoundReady` | On `vfio-pci`, IOMMU group clean | Ready for passthrough |

`probe_host_nvidia_vfio_readiness()` scans sysfs for vendor ID `0x10de`, checks the driver symlink, and inspects `/sys/bus/pci/devices/<addr>/iommu_group/devices/` for group cleanliness. Returns per-device readiness for multi-GPU hosts.

---

## Guest requirements (GPU path)

### Guest kernel (`openshell.kconfig` additions)

| Config | Purpose |
|---|---|
| `CONFIG_PCI`, `CONFIG_PCI_MSI` | PCIe device visibility and interrupts |
| `CONFIG_DRM` | GPU device node creation (`/dev/dri/*`) |
| `CONFIG_MODULES`, `CONFIG_MODULE_UNLOAD` | NVIDIA proprietary driver is a loadable module |
| `CONFIG_FB` / `CONFIG_FRAMEBUFFER_CONSOLE` | Optional: if GPU is the only display device |

Do **not** enable `CONFIG_VFIO` in the guest (no nested passthrough).

### Guest rootfs (GPU variant)

The GPU rootfs extends the base rootfs with:

- **NVIDIA kernel driver** matching the target GPU hardware generation
- **NVIDIA container toolkit** (`nvidia-container-toolkit`) so `nvidia-container-runtime` is available to containerd/k3s
- **`nvidia-smi`** for health checks

Distribution: separate `rootfs-gpu.tar.zst` artifact alongside the base `rootfs.tar.zst`. The launcher selects the GPU variant when `--gpu` is passed.

### Guest init (`openshell-vm-init.sh`)

When `GPU_ENABLED=true` is set in the environment:

1. Load NVIDIA kernel modules (`nvidia`, `nvidia_uvm`, `nvidia_modeset`)
2. Run `nvidia-smi` -- fail fast with a clear error if the device is not visible
3. Copy `gpu-manifests/*.yaml` (NVIDIA device plugin HelmChart CR) into the k3s auto-deploy directory
4. Verify `nvidia-container-runtime` is registered with containerd

When `GPU_ENABLED` is unset or false: no GPU paths execute (current behavior).

---

## CLI interface

### `--gpu`

```
openshell gateway start --gpu              # Auto-select first VFIO-ready GPU
openshell gateway start --gpu 0000:41:00.0 # Select specific PCI address
```

Errors:
- No NVIDIA PCI device found
- GPU not bound to `vfio-pci` (with instructions to bind)
- IOMMU group not clean (lists non-VFIO peers)
- GPU passthrough not supported on macOS
- cloud-hypervisor binary not found in runtime bundle

### Runtime bundle

```
~/.local/share/openshell/vm-runtime/{version}/
├── libkrun.so              # existing
├── libkrunfw.so.5          # existing
├── gvproxy                 # existing
├── provenance.json         # existing
├── cloud-hypervisor        # new (GPU path, ~10 MB, Linux only)
├── vmlinux                 # new (GPU path, ~15 MB, from libkrunfw build)
└── virtiofsd               # new (GPU path, ~5 MB)
```

`cloud-hypervisor`, `vmlinux`, and `virtiofsd` are only required for `--gpu` launches. Non-GPU launches do not validate their presence.

---

## Security model

GPU passthrough grants the guest **full device access** -- the same trust model as passing a GPU into the Docker cluster container today. The guest can issue arbitrary PCIe transactions to the device. IOMMU protects host memory from DMA attacks by the device, but the guest has unrestricted control of the GPU itself.

---

## Constraints and limitations

| Constraint | Impact | Mitigation |
|---|---|---|
| **Dual-backend maintenance** | Two VMM code paths for boot, networking, vsock, console | `VmBackend` trait limits blast radius; CI tests for both |
| **Linux-only GPU path** | macOS cannot use VFIO passthrough | macOS uses libkrun exclusively; GPU is out of scope for macOS |
| **NVIDIA FLR quirks** | Consumer GeForce may not reset on VM shutdown | Target data-center GPUs (A100, H100, L40) first; document supported list |
| **Single-GPU display loss** | Binding only GPU to `vfio-pci` removes host display | Document headless operation; recommend secondary GPU |
| **NVIDIA driver coupling** | Guest driver must match GPU generation | Pin driver version in rootfs; test against GPU matrix |
| **IOMMU group granularity** | Some boards group GPU with other devices | Recommend server hardware; document ACS override (unsupported) |
| **BAR size / MMIO** | Large-BAR GPUs need 64-bit MMIO support | Document BIOS settings (Above 4G Decoding, Resizable BAR) |
| **cloud-hypervisor NVIDIA issues** | Some driver failures reported upstream | Target data-center GPUs; pin cloud-hypervisor version |
| **GPU rootfs size** | NVIDIA driver + toolkit adds ~2-3 GB | Separate `rootfs-gpu.tar.zst` artifact |
| **Runtime bundle size** | cloud-hypervisor + vmlinux + virtiofsd add ~30 MB | Only in Linux GPU builds; separate tarball if needed |

---

## Related documents

- [Custom libkrun VM runtime](architecture/custom-vm-runtime.md) -- microVM layout, build pipeline, networking
- [Cluster bootstrap (Docker)](architecture/gateway-single-node.md) -- existing `--gpu` / `GPU_ENABLED` behavior
- [Implementation plan](vm-gpu-passthrough-implementation.md) -- phased work to build this
- [cloud-hypervisor VFIO docs](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/vfio.md) -- upstream VFIO reference
- [cloud-hypervisor REST API](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/api.md) -- programmatic VM management
- [rust-vmm/vfio](https://github.com/rust-vmm/vfio) -- VFIO Rust bindings used by cloud-hypervisor

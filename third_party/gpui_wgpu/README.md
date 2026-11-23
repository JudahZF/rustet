# gpui_wgpu (patched)

Zed's `gpui_wgpu` crate at commit `2de9e435a5d98657f35c260b5f4dd0979df659e6` (Zed v1.29.2, the
commit the root `Cargo.toml` pins gpui to), with one change. The root `Cargo.toml` puts it in the
build with:

```toml
[patch."https://github.com/zed-industries/zed"]
gpui_wgpu = { path = "third_party/gpui_wgpu" }
```

Every other Zed crate still comes from the git dependency. `src/` is the crate's `src/` at that
commit except for the change below; `LICENSE-APACHE` is Zed's licence for the crate.

## The change

`src/wgpu_context.rs`, `WgpuContext::instance` (the instance for native windows):

```diff
     pub fn instance(display: Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>>) -> wgpu::Instance {
         wgpu::Instance::new(wgpu::InstanceDescriptor {
             backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
-            flags: wgpu::InstanceFlags::default(),
+            // Rustet: `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=1` lets a Vulkan driver that does not
+            // claim conformance (Mesa's Asahi driver) be used (third_party/gpui_wgpu/README.md).
+            flags: wgpu::InstanceFlags::default().with_env(),
             backend_options: wgpu::BackendOptions::default(),
             memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
             display,
         })
     }
```

`InstanceFlags::with_env` reads wgpu's documented variables: `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER`,
and also `WGPU_VALIDATION`, `WGPU_DEBUG` and the other debugging switches. Unset, the flags are
the defaults, so nothing changes for anyone who does not set a variable.

## Why

On Apple Silicon Linux (Asahi), Mesa's Vulkan driver works but this Mesa build does not claim
conformance, so wgpu leaves it out unless `ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER` is set, and
gpui falls back to OpenGL. On that OpenGL path every image upload costs 25–30 ms of the main
thread whatever its size: a music video showed about 20 frames a second at one full core. Through
Vulkan the same video shows at its full rate (numbers in the `video` scenario's reports).

The app decides when to set the variable (`crates/rustet/src/main.rs`, `prepare_ui_process`): on
Apple Silicon Linux, read from the device tree, unless the user set it already (`0` keeps
OpenGL). Elsewhere nothing changes. If Vulkan fails to start, gpui falls back to OpenGL as
before (it tests each adapter in turn).

## Manifest

The crate's `Cargo.toml` stands alone: Zed's workspace-inherited fields are written out with the
values Zed's root `Cargo.toml` has at the pinned commit, and Zed's own crates (`gpui`,
`collections`, `gpui_util`) are git dependencies at the same revision, so they resolve to the
packages the rest of the build uses (`cargo tree -d` shows no second copy). The benches and their
dev-dependencies are left out. The crate is not a member of Rustet's workspace: `cargo clippy
--workspace` does not lint it.

## Moving the pin

When the gpui pin in the root `Cargo.toml` moves to another Zed commit:

1. Check whether the copy is still needed: if gpui's `WgpuContext::instance` reads the
   variables itself (`with_env`, or a setting for non-compliant adapters), or if Mesa reports
   conformance for the Asahi driver (the app's start-up log then names the Vulkan adapter
   without the variable), delete this directory and the `[patch]` entry.
2. Otherwise, replace `src/` with the crate's `src/` at the new commit and apply the change
   above again.
3. Compare the crate's `Cargo.toml` at the new commit with this one: carry over new or changed
   dependencies, taking the versions from Zed's root `Cargo.toml` at that commit, and set the
   new revision on the git dependencies (Zed's crates and wgpu).
4. Build, then `cargo tree -d` (no second copy of a Zed crate or of wgpu) and the `video`
   scenario on Wayland: its report names the adapter (`"gpu"`).

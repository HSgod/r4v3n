//! Ahead-of-time shader translation and compute pipeline precompilation into
//! the exact same on-disk caches [`crate::pipeline`] uses at run time:
//!
//! 1. `cache_dir/shaders/` ([`crate::shader_cache`]): translated DXIL + binding
//!    metadata for every supported stage (VS, PS, MS, CS), keyed identically to
//!    a live run (same executable hash, GPU wave sizes and translation switches).
//! 2. `cache_dir/pipelines/host-pipelines.bin` ([`crate::pipeline::host`]):
//!    compiled host D3D12 compute PSOs (and GPU driver shader cache entries),
//!    sharing the exact root-signature builder and pipeline-library key with
//!    run-time pipeline creation.
//!
//! Ported from `vitutu1/victory-perf` (GPL-3.0-or-later).

use victory_shader::HwStage;
use victory_shader::emit::{HostShader, Target};

/// Properties of the host GPU detected during initialization.
#[derive(Debug, Clone, Copy)]
pub struct HostInfo {
    pub target: Target,
    pub mesh_shaders: bool,
}

/// One translated shader ready for optional compute pipeline precompilation.
pub struct PreparedShader {
    shader: HostShader,
    pub stage: &'static str,
    pub is_compute: bool,
    /// `true` when loaded from the on-disk shader cache, `false` when freshly
    /// translated and written to disk.
    pub cached: bool,
}

/// Initializes the host D3D12 device and probes its wave sizes and mesh shader
/// support so offline translations produce the exact same DXIL and cache keys
/// as `victory run`.
pub fn init_host() -> Result<HostInfo, String> {
    let host = crate::host::host().ok_or_else(|| "no host D3D12 device available".to_owned())?;
    crate::descriptor::host::probe_view_limit();
    Ok(HostInfo {
        target: host.target(),
        mesh_shaders: host.mesh_shaders(),
    })
}

/// Translates one console shader container into the runtime shader cache (or
/// reads it back if already cached).
pub fn shader(container: &[u8], target: Target) -> Result<PreparedShader, String> {
    let (shader, cached) = crate::shader_cache::host_shader_cached(container, target)?;
    let stage = stage_name(shader.stage);
    let is_compute = shader.stage == HwStage::Cs;
    Ok(PreparedShader {
        shader,
        stage,
        is_compute,
        cached,
    })
}

/// Compiles a prepared compute shader into the host pipeline library (and GPU
/// driver cache). Returns `Ok(true)` if loaded from the library, `Ok(false)` if
/// freshly compiled and stored.
pub fn compute_pipeline(prepared: &PreparedShader) -> Result<bool, String> {
    if !prepared.is_compute {
        return Err(format!("not a compute shader (stage {})", prepared.stage));
    }
    crate::pipeline::host::precompile_compute(&prepared.shader)
}

/// Flushes the host pipeline library (`host-pipelines.bin`) to disk now if any
/// pipelines were added.
pub fn save_pipeline_cache() {
    crate::pipeline::save_pipeline_cache();
}

fn stage_name(stage: HwStage) -> &'static str {
    match stage {
        HwStage::Vs => "vertex",
        HwStage::Ps => "pixel",
        HwStage::Ms => "mesh",
        HwStage::Cs => "compute",
        HwStage::Gs => "geometry",
        HwStage::Hs => "hull",
        HwStage::Ls => "domain",
        HwStage::Es => "export",
    }
}

mod rig;
use rig::Rig;

/// Draws in a batch.
pub(super) const DRAWS: u32 = 10_000;
pub(super) const WIDTH: u32 = 2560;
pub(super) const HEIGHT: u32 = 1440;

/// One small triangle in the middle of the target (about 180 pixels),
/// and two pixel programs that read one texel of a texture through a
/// table, so that the view heap set is in use.
pub(super) const DRAW: &str = r#"
#define RS "DescriptorTable(SRV(t0))"
Texture2D<float4> image : register(t0);
[RootSignature(RS)]
float4 corner(uint i : SV_VertexID) : SV_Position {
float2 p = float2((i << 1) & 2, i & 2);
return float4(p * 0.02 - 0.01, 0.5, 1.0);
}
[RootSignature(RS)]
float4 first(float4 at : SV_Position) : SV_Target {
return image.Load(int3(0, 0, 0));
}
[RootSignature(RS)]
float4 second(float4 at : SV_Position) : SV_Target {
return 1.0 - image.Load(int3(0, 0, 0));
}
"#;

/// A compute pass of next to nothing, as our clamp of an indirect
/// record is: one group of one thread that writes one word.
pub(super) const WORK: &str = r#"
#define RS "UAV(u0)"
RWByteAddressBuffer words : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void work() {
words.Store(0, words.Load(0) + 1);
}
"#;

/// What changes between every two draws of a batch.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Between {
    /// Nothing.
    Nothing,
    /// The pipeline: two alternate.
    Pipeline,
    /// A second texture goes from target to shader read and back.
    BarrierPair,
    /// The depth texture bound for testing goes from write to read or
    /// back.
    DepthMove,
    /// The pair of descriptor heaps: two pairs alternate.
    Heaps,
    /// The colour target: two alternate.
    Targets,
    /// A compute pass of ours of one group, and the pipeline back.
    Compute,
    /// The same between two waits for every earlier shader write, as
    /// the clamp records them.
    ComputeWithWaits,
}


#[test]
#[ignore = "a measurement on the card: run it by name with --ignored --nocapture"]
fn bare_command_cost() {
    let Some(mut rig) = Rig::new() else {
        eprintln!("no hardware adapter, or our shaders did not compile; skipped");
        return;
    };
    eprintln!("bare command cost on {}: {DRAWS} one-triangle draws into {WIDTH}x{HEIGHT}, three batches each", rig.card);
    // The first batch pays for whatever the driver does at first use.
    rig.batch(Between::Nothing, 1);
    let variants = [
        ("(a) nothing changes between draws", Between::Nothing, 1),
        ("(b) another pipeline every draw", Between::Pipeline, 1),
        ("(c) a barrier pair on a second texture every draw", Between::BarrierPair, 1),
        ("(d) the tested depth texture moved between write and read every draw", Between::DepthMove, 1),
        ("(e) another pair of descriptor heaps every draw", Between::Heaps, 1),
        ("(f) another colour target every draw", Between::Targets, 1),
        ("(g) nothing changes, in 100 lists run one by one", Between::Nothing, 100),
        ("(h) a compute pass of ours of one group before every draw", Between::Compute, 1),
        ("(i) the same pass between two waits for earlier shader writes", Between::ComputeWithWaits, 1),
    ];
    let mut least = Vec::new();
    for (name, between, lists) in variants {
        let mut runs: Vec<(f64, f64)> = (0..3).map(|_| rig.batch(between, lists)).collect();
        runs.sort_by(|a, b| a.0.total_cmp(&b.0));
        eprintln!(
            "{name}: {:.2} microseconds a draw on the card (the three: {:.2}, {:.2}, {:.2}); recording took {:.1} ms",
            runs[0].0, runs[0].0, runs[1].0, runs[2].0, runs[0].1
        );
        least.push(runs[0].0);
    }
    assert!(least.iter().all(|us| us.is_finite() && *us > 0.0), "the card gave no time for a batch");
}

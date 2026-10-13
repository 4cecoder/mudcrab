# Offline object LOD comparison

This is the first converter experiment from the
[#203 hybrid agreement](https://github.com/Mudcrab-Team/mudcrab/issues/203#issuecomment-6086560142).
It builds conventional index LODs and a grouped cluster DAG from the same
eligible GLB primitive. Source GLBs stay unchanged. Nothing in the engine or
normal converter reads these artifacts; this is not a runtime sidecar ABI.

The adapter uses meshoptimizer v1.3 at commit
`9e1f07b159d3cb777f1c67ed31fc11fd117986f4`, including
[`demo/clusterlod.h`](https://github.com/zeux/meshoptimizer/blob/9e1f07b159d3cb777f1c67ed31fc11fd117986f4/demo/clusterlod.h).
meshoptimizer is MIT licensed. The build copies its license beside the binary;
keep that notice when distributing a built adapter.

## Build and synthetic run

Requirements: Python 3.10 or newer, Git, a C++17 compiler and a clean local
checkout at the exact pin. The build performs no network requests. It checks
the checkout identity, reads upstream source from immutable Git objects, and
compiles a private snapshot in one sequential compiler invocation. Choose a
new binary path outside the upstream checkout.

```sh
python3 scripts/lod-experiment/build.py \
  --meshoptimizer /path/to/meshoptimizer-v1.3 \
  --output /tmp/mudcrab-lod-builder

mkdir /tmp/lod-input
python3 scripts/lod-experiment/synthetic.py \
  --output /tmp/lod-input/grid.glb --size 24

python3 scripts/lod-experiment/experiment.py \
  --input /tmp/lod-input/grid.glb \
  --asset-root /tmp/lod-input \
  --output /tmp/lod-grid-results \
  --builder /tmp/mudcrab-lod-builder

env PYTHONDONTWRITEBYTECODE=1 LOD_EXPERIMENT_BUILDER=/tmp/mudcrab-lod-builder \
  python3 -m unittest discover -s scripts/lod-experiment/tests -v
```

The output must be a new directory disjoint from the asset root. The synthetic
writer also requires a new file. No generated fixtures or game assets belong
in Git.

`build.py` writes the binary, `<binary>.build.json` and `<binary>.license.txt`.
The receipt identifies the pin, adapter source, cluster header, compiled
binary, compiler and flags. The runner verifies the receipt, executes a
snapshot of that binary, and rechecks inputs, receipt, binary and driver before
publication. Input changes abort the run without publishing mixed candidates.

## Existing converted models

Point `--input` at a GLB or directory. Directory discovery sorts paths and
selects at most 32 models by default; `--limit` changes that cap, and `--limit 0`
selects all. The manifest states discovered/selected counts and whether the
inventory was truncated. Use a small explicit sample before a larger run.

```sh
python3 scripts/lod-experiment/experiment.py \
  --input /path/to/modern_assets/meshes/architecture \
  --asset-root /path/to/modern_assets \
  --output /tmp/lod-architecture-sample \
  --builder /tmp/mudcrab-lod-builder --limit 3
```

Set `--asset-root` to the converted package root when image URIs traverse from
meshes into its texture directory. The runner hashes referenced local images
without decoding them; it rejects external URLs, escaping paths and symlinked
asset lookups. GLB bytes and all consumed dependencies are rechecked before
publication. It does not lock a running converter's output: use an immutable
source package rather than a directory being regenerated.

## Results and checks

`manifest.json` and `candidates/*.json` contain deterministic metadata and
index candidates for the same inputs, settings and built binary. Timings live
separately in `measurements.json`. Exit 0 permits explicit omissions; exit 1
reports failed files/primitives in a published diagnostic result; preflight or
input-boundary failures exit 2 without publishing a result. An omitted category
is not successful generation.

Each candidate contains three conventional ratios (1/2, 1/4 and 1/8), the
cluster groups/indices, source vertex/accessor/material/node mappings and
complete uniform-depth cuts. Original clusters map to source triangle IDs;
the source cut must match every original oriented triangle exactly once.
Coarse clusters retain original vertex indices and group-refinement links.
They do not claim a per-coarse-triangle mapping to original faces.

The validator checks acyclic refinement depths, whole-group replacement,
terminal groups, finite/nonnegative bounds/errors, emitted-vertex sphere
containment, requested cluster size and valid index ranges. Its independent
cut rewriting must agree with upstream's depth/terminal selection rule.
These checks establish structural cuts; they do not certify cracks under
arbitrary mixed/view cuts, rendered appearance or conservative shape error.

Both routes reuse all source vertex data and the same decoded attributes and
open-boundary vertex locks. Every supported attribute component has weight
1.0. Cluster generation starts with `clodDefaultConfig`, then disables
permissive/sloppy fallbacks, error clamping and border dilation, and enables
fold preservation. Conventional targets have an unbounded error limit and
may fail to reach their requested ratio. Errors are meshoptimizer estimates
in model coordinates, including attribute weighting; they are **not certified
conservative bounds** or matched visual-quality acceptance.

Measurements include each generation stage, whole adapter process time, JSON
artifact bytes and separate theoretical 32-bit index-stream bytes. Vertices,
textures and runtime GPU allocations are not duplicated into these artifacts.
JSON size is not a proposed packed sidecar size. One local run, especially on
a busy host, does not establish throughput, parallel scaling or frame gains.

## Scope limits

- Only rigid, opaque triangle-list primitives with an embedded buffer qualify.
  The default small-primitive cutoff equals `--max-cluster-triangles` (128).
  Supported attributes are float32 positions/normals/tangents, UV sets and
  `COLOR_0`; normalized unsigned-byte/unsigned-short UV/color inputs work.
- Alpha-tested/blended, skinned, animated, morph, sparse, compressed and
  unsupported material/attribute routes produce explicit omissions. The first
  experiment also omits degenerate triangles and non-manifold position edges.
  Material extensions are restricted to `KHR_materials_emissive_strength`
  and `KHR_materials_specular`; their original data and image hashes survive.
- Eligibility here describes model geometry. It does not inspect winning
  reference records, `STAT` type, disabled flags, XESP or runtime mutations.
  Those remain prerequisites for admitting these models as world object LOD.
- The runner caps GLBs at 128 MiB and accessor counts at one million, and
  applies a 120-second timeout per primitive. It does not compute a reusable
  cache, page hierarchy, spatial HLOD, view cuts, indirect commands or a renderer.
- It records index candidates rather than exportable conventional GLBs. Native
  visual comparison, silhouette/material error, peak memory, backend behavior,
  cache acceptance and performance/default promotion remain later gates in
  [the hybrid contract](../../docs/specs/engine/hybrid-object-lod.md).

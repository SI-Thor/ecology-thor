# SAFETY.md — the memory-safety review policy

*"We don't like memory leaks… review closely, not just the Cpp work, but
all code — the AGX Thor is delicate in some areas." This page is how
that instruction became policy.*

## The posture

The kernel runs **24/7 on a box with 122 GB of usable RAM, no reboot
schedule, and a shared GPU**. A leak is not a crash — it is a slow
suffocation of the world builder. A use-after-free is not a segfault —
it is a *lie in the necrology*. So the bar is: no leaks by construction,
no unsafe without a written invariant, no dependency to audit but our
own code.

## Layer by layer

### `ecology-core` (the kernel) — zero dependencies, one `unsafe`

- **Zero dependencies is a safety feature.** `Cargo.toml` is empty on
  purpose: every line that runs in the real-time loop is in this repo,
  reviewable, with no supply chain between us and the semantics. (The
  `unsafe`-free parts of the crate are `miri`-clean by construction.)
- **No allocation in steady state.** Every table is preallocated to
  `Config::capacity`; the journal ring, scratch buffers and cascade stack
  reuse capacity. A "leak" would require calling `spawn` until the arena
  refuses (`SpawnError::Capacity`) — and then it *stops*, loudly, in the
  API, not silently in the allocator.
- **No `Drop` surprises.** Everything is plain data (`Vec` of PODs);
  nothing implements `Drop` but the containers themselves.
- **The one `unsafe`**: the seqlock cell (`snapshot.rs`). Invariant,
  written at the `unsafe impl Send/Sync`: single writer (the tick
  thread, enforced by `&mut self` ownership of `Ecology`), POD payload,
  odd-seq exclusion for readers. Exercised by a concurrent-reader test
  (`seqlock_roundtrip_under_concurrent_writes`). Review rule: any new
  `unsafe` in this crate requires an invariant comment of the same shape
  and a test that would catch its violation.

### `ecology-cuda` — the CUDA/C++ seam, contained

The C++ surface is **one file** (`kernels/ecology.cu`) and it contains:
no allocation (`malloc`/`new` appear nowhere; kernels write caller-owned
arrays), no recursion, no dynamic shared memory, no host-side state at
all — kernels are pure column transforms. That is the reviewable seam.

The host shim (`device.rs`) is Rust over cudarc:

- All device memory is owned by `UnifiedSlice` (RAII; freed on `Drop`).
- All modules/functions are owned by `CudaModule`/`CudaFunction`
  (unloaded on `Drop`).
- Every `unsafe` block carries a SAFETY comment naming the length
  invariant of every pointer passed (see `device.rs`).
- Every CUDA call is checked (`?` propagation); there is no silent
  error path. `compute-sanitizer --tool memcheck|leakcheck` works on
  Tegra (forum-verified) and belongs in the on-device CI ritual
  (`make test-device`).
- cudarc is `dynamic-loading`: nothing links at build time on machines
  without CUDA; the wrong-driver failure mode is a clean runtime error.

### `ecology-sim` / `ecology-py` — edges, not kernel

The simulator may allocate (it's a workload model, not the loop). The
pyo3 bindings hold the kernel by `&mut self` (pyo3's borrow discipline
is exactly our single-writer law) — two Python threads cannot mutate the
organism concurrently, by construction.

## Review checklist (every PR)

1. `make guard` green (fmt, `clippy -D warnings`, tests, secret scan).
2. `cargo tree -p ecology-core` prints **no dependencies** — or the PR
   explains why the zero-dep policy is being changed in ARCHITECTURE.md.
3. New `unsafe`? Invariant comment + a test that catches its violation.
4. New kernel or float path? The three implementations (ledger /
   reference / kernel) change **together**, textually aligned; the parity
   test is updated in the same PR.
5. Anything touching the journal encoding? `BLESS=1` the golden hash
   deliberately, with the reason in the commit message.
6. Anything touching C++? grep the diff for `new`, `delete`, `malloc`,
   `free`, `cudaMalloc` — each hit must justify itself or leave.
7. Benchmarks quoted in docs carry the measurement context (PLACEMENT.md
   rule).

## Secret hygiene

Keys never ride in commits (`tools/check_secrets.py`, wired into `make
guard` and CI). If the scanner fires: **rotate first**, then clean
history. The scan patterns cover the token shapes this team actually
uses (GitHub PATs, typesafe.ai, DeepInfra, plus generic AWS/bearer/
private-key shapes).

## Known sharp edges (kept visible, not hidden)

- The seqlock read can return `None` under pathological contention
  (bounded retries); callers back off one tick. This is a feature (the
  writer is never blocked), documented in `snapshot.rs`.
- `Rails` is `f32` money on purpose (admission envelopes, not
  accounting truth — `rails.rs` header). If someone wires it to real
  billing, that decision gets revisited here.
- The arena never shrinks: a pathological world that spawns forever will
  hit `Capacity` and stay there until restart. The journal records every
  spawn, so the post-mortem (and the replay) knows exactly what grew.
- CUDA submission from SCHED_FIFO threads has crashed Orin-class kernels
  in the field (RT.md §3.4). The tick thread may be RT; the submit
  helper is not, until re-verified on Thor.

# CONTRIBUTING.md

## The gates

Every PR passes `make guard`:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # includes the golden determinism hash
python3 tools/check_secrets.py
```

CI runs the same, plus an aarch64 `cargo check` (the Thor's target), a
headless PTX-assembly gate, a CUDA-container compile of the kernels, and
a compile check of the Python bindings. Device parity
(`cargo test -p ecology-cuda --features cuda`) runs on the Thor, not in
the cloud — there is no sm_110 in GitHub's fleet.

## The house rules

1. **`ecology-core` stays dependency-free.** If a PR adds a dependency to
   the kernel crate, it is changing this project's memory-safety story
   (SAFETY.md) and must say so, loudly, in ARCHITECTURE.md too.
2. **Three implementations change together.** Ledger / CPU reference /
   CUDA kernel are the same function three times. A float-path PR that
   touches one and not the others is wrong by definition. Textually
   align the expressions; the parity test will hold you to it on device.
3. **Never reassociate the arithmetic.** The multiply order in
   `attention_score`, the Horner form of `exp2_fast`, the
   `-fmad=false` flag — these are the bit-exactness contract
   (PTX_NOTES.md). "Cleanups" that change bits break the golden hash,
   and that's the point: re-bless it consciously (`BLESS=1 cargo test
   -p ecology-core --test golden`), with the reason in the commit
   message, or don't.
4. **No new `unsafe` without its invariant.** Every `unsafe` block in
   this repo carries a SAFETY comment naming the invariant, and a test
   that would catch its violation. The core currently has exactly one
   (the seqlock); keeping that count low is a feature.
5. **C++ rules (the one file):** `kernels/ecology.cu` contains no
   allocation, no recursion, no dynamic shared memory, no host state.
   `new`, `delete`, `malloc`, `free`, `cudaMalloc` do not appear — keep
   it that way. Host-side ownership is Rust RAII (cudarc), everywhere.
6. **Placement claims are measurement claims** (PLACEMENT.md): numbers
   carry machine, population, power mode, clock state, or they don't get
   merged.
7. **Secrets never ride in commits.** If `check_secrets.py` fires:
   rotate the key first, then clean history. It's wired into `make
   guard` and CI as the belt to your braces.
8. **The journal is the record of why.** Decisions with a reason land in
   `docs/JOURNAL.md` — the code review is where the reason gets checked,
   not invented.

## Golden-hash procedure

The golden chain hash (`crates/ecology-core/tests/golden/chain_hash.txt`)
pins the kernel's replay contract. When a change intentionally alters
event encoding, ordering, or any semantics the chain folds:

```bash
BLESS=1 cargo test -p ecology-core --test golden
git add crates/ecology-core/tests/golden/chain_hash.txt
# commit message MUST say why the hash changed
```

An unintentional change fails CI with the diff in your face.

## On-device checklist (AGX Thor)

```bash
sudo tools/thor-tune.sh          # clocks pinned, power mode chosen
make kernels                     # nvcc -arch=native → sm_110 PTX
make test-device                 # bit-exact parity, per-kernel timings
compute-sanitizer --tool memcheck target/debug/...   # after parity
```

Record results (with context) in BENCHMARKS.md's Thor table and the
PLACEMENT crossover table.

## Branch/PR conventions

- `main` is always green (guard + CI).
- Proposal branches to sibling repos follow their conventions, not ours
  (e.g. builder-ide's ADR/corpus rules; infinigen-thor's `thor/`
  layout; thor-world's purity law — never import builder-ide from
  world-mcp code).

# ecology-thor — root Makefile
#
# Three contexts use this file:
#   * a dev laptop / CI (no CUDA): `make guard` covers everything buildable
#   * the AGX Thor: `make kernels` then `make test-device`
#   * release/push day: `make push-guard` (the triple check)

CARGO ?= cargo
PY ?= python3

.PHONY: all build test clippy fmt guard kernels ptx-check hand-ptx-check \
        test-device python bench bench-report secrets push-guard clean

all: build

build:
	$(CARGO) build --workspace --release

test:
	$(CARGO) test --workspace

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --all -- --check

# The everyday gate: format, lints, tests, secret scan.
guard: fmt clippy test secrets

# Device kernels (needs nvcc — on the Thor, or CI's CUDA container).
kernels:
	$(MAKE) -C crates/ecology-cuda/kernels all

ptx-check:
	$(MAKE) -C crates/ecology-cuda/kernels ptx-check

# Headless PTX assembly gate (needs only ptxas, e.g. from the pip wheel
# nvidia-cuda-nvcc-cu12; see docs/PTX_NOTES.md).
hand-ptx-check:
	$(MAKE) -C crates/ecology-cuda/kernels hand-ptx-check ECO_SM=sm_100

# On-device: full parity between CPU reference and the Blackwell kernels.
# Requires `make kernels` first (build/ecology.ptx) and libcuda present.
test-device:
	$(CARGO) test -p ecology-cuda --features cuda -- --nocapture

# Python bindings (build on the Thor, where CPython headers live).
python:
	cd crates/ecology-py && maturin build --release

# Benchmarks. Honest numbers need: release build, quiet machine, and on the
# Thor — fixed clocks (docs/BENCHMARKS.md has the full protocol).
bench: build
	mkdir -p data
	./target/release/ecology-sim --profile stress --capacity 10000 --ticks 30000 --seed 42 --report data/stress_10k.json
	./target/release/ecology-sim --profile stress --capacity 50000 --ticks 30000 --seed 42 --report data/stress_50k.json
	./target/release/ecology-sim --profile stress --capacity 100000 --ticks 30000 --seed 42 --report data/stress_100k.json
	./target/release/ecology-sim --profile world-gen --ticks 6000 --capacity 50000 --seed 42 --report data/worldgen_50k.json

# Secrets never ride in commits (docs/SAFETY.md). Scans staged + tracked files.
secrets:
	$(PY) tools/check_secrets.py

# The pre-push triple check.
push-guard: guard
	@echo "push-guard OK: fmt + clippy + tests + secrets all green"

clean:
	$(CARGO) clean
	$(MAKE) -C crates/ecology-cuda/kernels clean
	rm -rf data

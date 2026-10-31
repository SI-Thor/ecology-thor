# kernels/Makefile — build the ecology-thor device code.
#
# Two contexts build this file:
#
#   1. ON THE THOR (authoritative): CUDA 13.x at /usr/local/cuda targets
#      sm_110 natively. `make` produces build/ecology.ptx, which the host
#      shim (crates/ecology-cuda, feature `cuda`) loads via cuModuleLoadData.
#
#   2. IN CI (headless syntax/regs gate): an x86_64 nvcc (e.g. the pip wheel
#      nvidia-cuda-nvcc-cu12) compiles the same source to PTX and ptxas
#      assembles it. No GPU is present; the point is to fail the build on
#      any kernel-source or PTX error before it reaches the device.
#
# DETERMINISM FLAG: -fmad=false is load-bearing — it stops nvcc from
# contracting a*b+c into FMA, which would break bit-exact parity with the
# CPU reference (see docs/PTX_NOTES.md §Determinism). Do not "optimize".
#
# Usage:
#   make                  # build/ecology.ptx
#   make ptx-check        # assemble PTX + hand-written PTX with ptxas -v
#   make clean

CUDA_HOME ?= /usr/local/cuda
NVCC      := $(CUDA_HOME)/bin/nvcc
PTXAS     := $(CUDA_HOME)/bin/ptxas

# Thor is sm_110 (Blackwell, compute capability 11.0). Neighbors for CI:
#   sm_90 (Hopper), sm_87 (Orin/Ampere), sm_120 (DGX Spark class).
ECO_ARCH  ?= compute_110
ECO_SM    ?= sm_110

BUILD     := build
PTX       := $(BUILD)/ecology.ptx
HAND_PTX  := ptx/warp_max_f32.ptx

CUDFLAGS  := -O3 -fmad=false -lineinfo --use_fast_math=false \
	     -Xcompiler -fno-strict-aliasing

.PHONY: all ptx-check clean

all: $(PTX)

$(BUILD):
	mkdir -p $(BUILD)

# Compile .cu → PTX (device code only; host never links this in CI).
$(PTX): ecology.cu | $(BUILD)
	$(NVCC) -ptx -arch=$(ECO_ARCH) $(CUDFLAGS) -o $@ $<

# Assemble both the compiled PTX and the hand-written microkernel, with
# register/spill reporting. `ptxas -v` output is the CI artifact we read.
#
# NOTE ON ARCH: the Thor is sm_110, which needs CUDA 13's ptxas. The pip
# wheel nvidia-cuda-nvcc-cu12 (12.9) used for headless CI knows up to
# sm_100 - and the hand-written PTX deliberately targets sm_90 (portable
# floor for the integer redux; the driver JITs it for sm_110 on device).
# So: headless CI runs `make hand-ptx-check ECO_SM=sm_100 PTXAS=...`;
# device builds run `make ptx-check` with the full CUDA 13 toolchain.
ptx-check: $(PTX) $(HAND_PTX)
	$(PTXAS) -arch=$(ECO_SM) -v -o $(BUILD)/ecology.cubin $(PTX)
	$(PTXAS) -arch=$(ECO_SM) -v -o $(BUILD)/warp_max_f32.cubin $(HAND_PTX)
	@echo "ptx-check OK (see register counts above)"

# Headless variant: no nvcc needed - only assembles the hand-written PTX
# (and any pre-built .ptx that exists). This is what the wheel-based CI job
# runs when the CUDA container is unavailable.
hand-ptx-check: $(HAND_PTX) | $(BUILD)
	$(PTXAS) -arch=$(ECO_SM) -v -o $(BUILD)/warp_max_f32.cubin $(HAND_PTX)
	@echo "hand-ptx-check OK (see register counts above)"

clean:
	rm -rf $(BUILD)

//! # ecology-cuda — the device side of the ecology-thor kernel.
//!
//! Two layers, one contract:
//!
//! * [`reference`] — CPU mirrors of every kernel, bit-exact, always built.
//!   They are the reviewable spec of what the device computes, the
//!   no-GPU execution path, and the parity anchor.
//! * [`device`] (feature `cuda`) — the cudarc host shim: unified-memory
//!   hot tables (`cuMemAllocManaged` — zero-copy on the Thor's shared
//!   LPDDR5X), PTX module loading, stream launches, and read-backs.
//!
//! The CUDA C++ kernels themselves live in [`kernels/`] and are built to
//! PTX by `kernels/Makefile` (`make kernels`), either on the Thor
//! (sm_110) or in CI's CUDA container. `docs/PTX_NOTES.md` documents the
//! bit-exactness policy (-fmad=false, canonical multiply order, shared
//! exp2 polynomial) and `docs/PLACEMENT.md` the who-computates-what map.
//!
//! Build matrix:
//!
//! | Context | Command | What runs |
//! |---|---|---|
//! | Any machine, no CUDA | `cargo test -p ecology-cuda` | reference mirrors vs. kernel semantics |
//! | CI, headless | `make -C crates/ecology-cuda/kernels hand-ptx-check` | PTX assembly gate |
//! | On the Thor | `make kernels && cargo test -p ecology-cuda --features cuda` | full device parity (`tests/parity.rs`) |

pub mod reference;

#[cfg(feature = "cuda")]
pub mod device;

#[cfg(feature = "cuda")]
pub use device::{DeviceError, DeviceSweeper};

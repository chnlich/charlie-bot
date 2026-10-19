//! `src/features/ncu`: the Nsight Compute report viewer at `GET /ncu`.
//!
//! NVIDIA's `ncu_report` module reads a `.ncu-rep` report, and it ships only for Python. A cache miss runs
//! `rust/helpers/ncu_report_helper.py` in the checkout's interpreter. When the module does not load, the native CSV
//! path reads `ncu --import <file> --csv --page details` instead.

mod api;
pub mod csv_import;
mod csv_reader;
mod discovery;
pub mod error;
mod pycompat;
mod roofline;
#[cfg(test)]
mod test_support;
mod viewer;

use charliebot_registry::{Registry, RegistryError};

pub fn register(registry: &mut Registry) -> Result<(), RegistryError> {
  registry.router(api::router, api::matches)?;
  Ok(())
}

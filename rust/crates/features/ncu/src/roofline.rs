//! `_collect_roofline`: the SpeedOfLight roofline points, read from the metrics of each kernel.
//!
//! The metric names and the derived definitions come from NVIDIA's `SpeedOfLight_RooflineChart` section. The chart
//! point is achieved FLOP/s = work per elapsed SM cycle times SMSP cycles per second, and arithmetic intensity is that
//! divided by achieved DRAM bytes per second. The float operations run in the order of the Python code, so the sums
//! round the same way.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::error::ParseError;

const PEAK_SM_CLOCK: &str = "sm__cycles_elapsed.avg.per_second";
const ACHIEVED_SM_CLOCK: &str = "smsp__cycles_elapsed.avg.per_second";
const PEAK_TRAFFIC_PER_CYCLE: &str = "dram__bytes.sum.peak_sustained";
const ACHIEVED_TRAFFIC: &str = "dram__bytes.sum.per_second";
const DRAM_CLOCK: &str = "dram__cycles_elapsed.avg.per_second";
const PLACEHOLDER: &str = "No roofline data in this report (requires --set full/detailed)";
const SOURCE: &str = "NCU SpeedOfLight_RooflineChart metrics";

/// One roofline chart: the peak-work metric, and the achieved-work metrics that sum into the point's work.
struct Definition {
  id: &'static str,
  label: &'static str,
  precision: &'static str,
  peak_work: &'static str,
  required_work: &'static [&'static str],
  optional_work: &'static [&'static str],
}

const DEFINITIONS: &[Definition] = &[
  Definition {
    id: "fp32",
    label: "Single Precision Roofline",
    precision: "FP32",
    peak_work: "derived__sm__sass_thread_inst_executed_op_ffma_pred_on_x2",
    required_work: &[
      "smsp__sass_thread_inst_executed_op_fadd_pred_on.sum.per_cycle_elapsed",
      "smsp__sass_thread_inst_executed_op_fmul_pred_on.sum.per_cycle_elapsed",
      "derived__smsp__sass_thread_inst_executed_op_ffma_pred_on_x2",
    ],
    optional_work: &[
      "derived__smsp__sass_thread_inst_executed_op_fadd2_pred_on_x2",
      "derived__smsp__sass_thread_inst_executed_op_fmul2_pred_on_x2",
      "derived__smsp__sass_thread_inst_executed_op_ffma2_pred_on_x4",
    ],
  },
  Definition {
    id: "fp64",
    label: "Double Precision Roofline",
    precision: "FP64",
    peak_work: "derived__sm__sass_thread_inst_executed_op_dfma_pred_on_x2",
    required_work: &[
      "smsp__sass_thread_inst_executed_op_dadd_pred_on.sum.per_cycle_elapsed",
      "smsp__sass_thread_inst_executed_op_dmul_pred_on.sum.per_cycle_elapsed",
      "derived__smsp__sass_thread_inst_executed_op_dfma_pred_on_x2",
    ],
    optional_work: &[],
  },
];

/// One chart point: the kernel and its ceilings. A value that is not finite is `None`, as `_clean_number` makes it.
struct Point {
  kernel_idx: Value,
  kernel_name: Value,
  achieved_flops: Option<f64>,
  arithmetic_intensity: Option<f64>,
  achieved_traffic_bytes_s: Option<f64>,
  compute_ceiling_flops: Option<f64>,
  memory_ceiling_bytes_s: Option<f64>,
  achieved_work_flop_per_cycle: Option<f64>,
  peak_compute_flop_per_cycle: Option<f64>,
}

impl Point {
  fn to_json(&self) -> Value {
    json!({
      "kernel_idx": self.kernel_idx,
      "kernel_name": self.kernel_name,
      "achieved_flops": self.achieved_flops,
      "arithmetic_intensity": self.arithmetic_intensity,
      "achieved_traffic_bytes_s": self.achieved_traffic_bytes_s,
      "compute_ceiling_flops": self.compute_ceiling_flops,
      "memory_ceiling_bytes_s": self.memory_ceiling_bytes_s,
      "achieved_work_flop_per_cycle": self.achieved_work_flop_per_cycle,
      "peak_compute_flop_per_cycle": self.peak_compute_flop_per_cycle,
    })
  }
}

/// `_collect_roofline(kernels)` for kernels whose `metrics` list holds `{name, value, unit}` entries.
pub fn collect(kernels: &[Value]) -> Result<Value, ParseError> {
  let mut rooflines = Vec::new();
  for definition in DEFINITIONS {
    let points: Vec<Point> = kernels.iter().filter_map(|kernel| point(definition, kernel)).collect();
    if points.is_empty() {
      continue;
    }
    let compute: Vec<Option<f64>> = points.iter().map(|point| point.compute_ceiling_flops).collect();
    let memory: Vec<Option<f64>> = points.iter().map(|point| point.memory_ceiling_bytes_s).collect();
    let source_metrics: Vec<&str> =
      required_metric_names(definition).chain(definition.optional_work.iter().copied()).collect();
    rooflines.push(json!({
      "id": definition.id,
      "label": definition.label,
      "precision": definition.precision,
      "compute_ceiling_flops": python_max(&compute)?,
      "memory_ceiling_bytes_s": python_max(&memory)?,
      "source_metrics": source_metrics,
      "points": points.iter().map(Point::to_json).collect::<Vec<_>>(),
    }));
  }
  Ok(json!({
    "available": !rooflines.is_empty(),
    "message": if rooflines.is_empty() { Value::String(PLACEHOLDER.to_string()) } else { Value::Null },
    "source": SOURCE,
    "rooflines": rooflines,
  }))
}

/// The metrics a point needs, in the order `required_metric_names` lists them.
fn required_metric_names(definition: &Definition) -> impl Iterator<Item = &str> {
  [definition.peak_work, PEAK_SM_CLOCK, ACHIEVED_SM_CLOCK, PEAK_TRAFFIC_PER_CYCLE, DRAM_CLOCK, ACHIEVED_TRAFFIC]
    .into_iter()
    .chain(definition.required_work.iter().copied())
}

/// The point of one kernel, or `None` when a required metric is missing or a rate is not positive.
fn point(definition: &Definition, kernel: &Value) -> Option<Point> {
  let metrics = numeric_metrics(kernel);
  let required: HashMap<&str, f64> = required_metric_names(definition)
    .map(|name| metrics.get(name).map(|value| (name, *value)))
    .collect::<Option<_>>()?;
  let peak_compute = required[definition.peak_work];
  let peak_sm_clock = required[PEAK_SM_CLOCK];
  let achieved_sm_clock = required[ACHIEVED_SM_CLOCK];
  let peak_traffic_per_cycle = required[PEAK_TRAFFIC_PER_CYCLE];
  let dram_clock = required[DRAM_CLOCK];
  let achieved_traffic = required[ACHIEVED_TRAFFIC];
  if peak_compute <= 0.0 || peak_sm_clock <= 0.0 || peak_traffic_per_cycle <= 0.0 || dram_clock <= 0.0 {
    return None;
  }
  if achieved_sm_clock <= 0.0 || achieved_traffic <= 0.0 {
    return None;
  }

  let mut achieved_work = 0.0;
  for name in definition.required_work {
    achieved_work += required[name];
  }
  for name in definition.optional_work {
    if let Some(value) = metrics.get(name) {
      achieved_work += value;
    }
  }
  let achieved_flops = achieved_work * achieved_sm_clock;
  let compute_ceiling = peak_compute * peak_sm_clock;
  let memory_ceiling = peak_traffic_per_cycle * dram_clock;
  Some(Point {
    kernel_idx: kernel["idx"].clone(),
    kernel_name: kernel["name"].clone(),
    achieved_flops: clean(achieved_flops),
    arithmetic_intensity: clean(achieved_flops / achieved_traffic),
    achieved_traffic_bytes_s: clean(achieved_traffic),
    compute_ceiling_flops: clean(compute_ceiling),
    memory_ceiling_bytes_s: clean(memory_ceiling),
    achieved_work_flop_per_cycle: clean(achieved_work),
    peak_compute_flop_per_cycle: clean(peak_compute),
  })
}

/// The finite numeric metrics of a kernel's `metrics` list, by name. A string or a null value is not numeric.
fn numeric_metrics(kernel: &Value) -> HashMap<&str, f64> {
  kernel["metrics"]
    .as_array()
    .into_iter()
    .flatten()
    .filter_map(|metric| {
      let value = metric["value"].as_f64().filter(|value| value.is_finite())?;
      Some((metric["name"].as_str()?, value))
    })
    .collect()
}

fn clean(value: f64) -> Option<f64> {
  value.is_finite().then_some(value)
}

/// Python's `max` over the ceilings of one chart. One item is returned as it is, even `None`. Two or more compare, and
/// a `None` among them makes Python's comparison raise `TypeError`.
fn python_max(values: &[Option<f64>]) -> Result<Option<f64>, ParseError> {
  let Some((first, rest)) = values.split_first() else {
    unreachable!("a chart has at least one point");
  };
  if rest.is_empty() {
    return Ok(*first);
  }
  let mut best = *first;
  for value in rest {
    match (best, value) {
      (Some(current), Some(candidate)) if *candidate > current => best = Some(*candidate),
      (Some(_), Some(_)) => {}
      _ => {
        return Err(ParseError::Internal {
          kind: "TypeError",
          detail: "'>' not supported between instances of 'NoneType' and 'float'".to_string(),
        });
      }
    }
  }
  Ok(best)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn kernel(idx: u64, metrics: &[(&str, f64)]) -> Value {
    let metrics: Vec<Value> =
      metrics.iter().map(|(name, value)| json!({"name": name, "value": value, "unit": ""})).collect();
    json!({"idx": idx, "name": format!("kernel_{idx}"), "metrics": metrics})
  }

  fn full_metrics() -> Vec<(&'static str, f64)> {
    vec![
      ("derived__sm__sass_thread_inst_executed_op_ffma_pred_on_x2", 128.0),
      (PEAK_SM_CLOCK, 1.5e9),
      (ACHIEVED_SM_CLOCK, 1.25e9),
      (PEAK_TRAFFIC_PER_CYCLE, 64.0),
      (DRAM_CLOCK, 5.0e8),
      (ACHIEVED_TRAFFIC, 2.0e10),
      ("smsp__sass_thread_inst_executed_op_fadd_pred_on.sum.per_cycle_elapsed", 16.0),
      ("smsp__sass_thread_inst_executed_op_fmul_pred_on.sum.per_cycle_elapsed", 8.0),
      ("derived__smsp__sass_thread_inst_executed_op_ffma_pred_on_x2", 32.0),
    ]
  }

  #[test]
  fn a_kernel_with_every_metric_gives_one_fp32_point() {
    let roofline = collect(&[kernel(0, &full_metrics())]).unwrap();

    assert_eq!(roofline["available"], json!(true));
    assert_eq!(roofline["message"], Value::Null);
    let chart = &roofline["rooflines"][0];
    assert_eq!(chart["id"], "fp32");
    assert_eq!(chart["compute_ceiling_flops"], json!(128.0 * 1.5e9));
    assert_eq!(chart["memory_ceiling_bytes_s"], json!(64.0 * 5.0e8));
    let point = &chart["points"][0];
    assert_eq!(point["achieved_work_flop_per_cycle"], json!(16.0 + 8.0 + 32.0));
    assert_eq!(point["achieved_flops"], json!((16.0 + 8.0 + 32.0) * 1.25e9));
    assert_eq!(point["arithmetic_intensity"], json!((16.0 + 8.0 + 32.0) * 1.25e9 / 2.0e10));
  }

  #[test]
  fn a_kernel_missing_a_metric_gives_the_placeholder() {
    let metrics: Vec<(&str, f64)> = full_metrics().into_iter().filter(|(name, _)| *name != DRAM_CLOCK).collect();

    let roofline = collect(&[kernel(0, &metrics)]).unwrap();

    assert_eq!(roofline["available"], json!(false));
    assert_eq!(roofline["message"], json!(PLACEHOLDER));
    assert_eq!(roofline["rooflines"], json!([]));
  }

  #[test]
  fn python_max_compares_only_from_two_items_on() {
    assert_eq!(python_max(&[None]).unwrap(), None);
    assert_eq!(python_max(&[Some(1.0), Some(3.0), Some(2.0)]).unwrap(), Some(3.0));
    assert!(python_max(&[Some(1.0), None]).is_err());
  }
}

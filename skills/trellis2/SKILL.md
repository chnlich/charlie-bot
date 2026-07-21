---
name: trellis2
description: TRELLIS.2 G16 production compile strategy. Use when configuring or diagnosing compile settings for TRELLIS.2 training runs.
version: 1.0.0
---

# TRELLIS.2 G16 Compile Strategy

The 100k-step production config must enable compile. The one-time startup cost
amortizes around 35k steps, so a 300-step short run's total time does not
invalidate production compile. Diagnostics only decide whether to add dynamic
shape optimizations, not whether to compile at all.

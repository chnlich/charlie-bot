//! The synthetic prose, markdown, code and tool traffic that events draw from. Every sentence is written for this crate.
//! A run composes the block and output-line pools once, so an event costs one random choice per block.

use serde_json::{Value, json};

use crate::rng::Rng;

/// The number of composed blocks and tool-output lines of one run.
pub const BLOCK_POOL_SIZE: usize = 1500;
pub const OUTPUT_LINE_POOL_SIZE: usize = 800;

const ZH_SENTENCES: &[&str] = &[
  "我们先把请求入口拆成解析、校验和分发三段。",
  "这个缓存的失效规则目前只看时间，还需要加上版本号。",
  "测试数据要覆盖空字段、超长字段和重复的键。",
  "列表在窄屏上会把按钮挤到第二行，先看看间距怎么定。",
  "调度器每次只取一个待办，处理完再取下一个。",
  "日志里有两次相同的超时，重试之间可能缺少退避。",
  "示例文档还在用旧的参数名，改完后要再跑一遍检查。",
  "这一步的输出只是中间结果，不需要写进最终报告。",
  "没有命中索引时退回全表扫描，并把耗时记入统计。",
  "先整理现有分支，再决定哪些改动可以合并。",
  "页面一次取回全部记录，数据量大时应改成分页。",
  "默认值只写在一个地方，别在三个文件里各写一份。",
  "这个函数有三种返回情况，调用方只处理了两种。",
  "压缩后的响应可以复用，不必每次请求都重新编码。",
  "失败的任务会留下标记文件，下次启动时清理它。",
  "状态转移用表格写出来，每行列出前置条件和动作。",
  "新增字段默认为空，旧数据读出来也不会报错。",
  "这里的锁粒度太粗，读请求也要等写请求结束。",
  "时间显示应以用户的时区为准，而不是服务器的时区。",
  "最后把结果写回文件，并确认文件权限是六百。",
  "这个模块只做转换，不应该直接访问磁盘。",
  "输入框的提示文字太长，换行后会遮住下面的按钮。",
  "回归测试放在同一个文件里，名字要说清楚场景。",
  "上一轮的数字偏高，先找出是哪一个步骤引入的。",
];

const EN_SENTENCES: &[&str] = &[
  "The handler parses the request body once and passes the result down.",
  "Retries back off exponentially, with a cap of thirty seconds.",
  "The fixture covers empty fields, very long names and repeated keys.",
  "Two readers share the cache, so the writer must publish a new generation.",
  "A table scan is cheap at this size, but the index will matter later.",
  "Keep the public signature unchanged and move the branch inside the function.",
  "A stale lock file blocks the next run until its owner process exits.",
  "We take the median of fifty requests after five warm-up calls.",
  "The renderer escapes angle brackets before it writes the table.",
  "Errors are logged once at the boundary instead of at every layer.",
  "Each worker writes its own file, so no two writers share a path.",
  "The default timeout lives in one place and every caller reads it.",
  "Unit tests cover the boundary values; the integration test covers the flow.",
  "The summary lists the changed files, the commands run and the results.",
  "This change only moves code, so the behavior of the function stays the same.",
  "Please confirm the expected order before the two branches are merged.",
  "The memory peak stays flat once the cache reaches its size limit.",
  "Thread safety depends on the single writer guard taken at startup.",
  "The sidebar shows only active sessions and loads archived rows on demand.",
  "Remove the helper after the last caller moves to the new module.",
  "The chart reads the same counters that the status page reads.",
  "Pagination keeps each response small when a list has many rows.",
  "The retry budget resets when the run reaches a terminal state.",
  "A reviewer can rerun the command in the table and compare the output.",
];

const ZH_TOPICS: &[&str] = &[
  "缓存层整理",
  "调度器超时",
  "会话列表分页",
  "发布检查清单",
  "日志格式统一",
  "配置项迁移",
  "上传文件校验",
  "侧边栏布局",
  "备份与恢复",
  "指标面板",
  "权限边界梳理",
  "数据表迁移",
];

const EN_TOPICS: &[&str] = &[
  "cache layer cleanup",
  "scheduler timeout audit",
  "session list paging",
  "release checklist",
  "log format unification",
  "config key migration",
  "upload validation",
  "sidebar layout pass",
  "backup and restore",
  "metrics panel",
  "permission boundary review",
  "schema migration",
];

const CODE_SNIPPETS: &[(&str, &str)] = &[
  (
    "python",
    "def merge_rows(rows):\n    seen = set()\n    return [r for r in rows if not (r[\"id\"] in seen or seen.add(r[\"id\"]))]",
  ),
  ("rust", "fn retry_delay(attempt: u32) -> Duration {\n    Duration::from_millis(250u64 << attempt.min(7))\n}"),
  (
    "typescript",
    "export function pageOf<T>(items: T[], size: number): T[][] {\n  const pages: T[][] = [];\n  for (let i = 0; i < items.length; i += size) pages.push(items.slice(i, i + size));\n  return pages;\n}",
  ),
  ("bash", "cargo test --workspace --locked\ncargo clippy --workspace --all-targets -- -D warnings"),
  ("sql", "SELECT id, name FROM sessions WHERE status = 'active' ORDER BY updated_at DESC LIMIT 40;"),
];

const TOOL_NAMES: &[&str] = &["Read", "Bash", "Grep", "Edit", "Glob"];

/// The composed pools of one run. Events pick from them, so the pools hold the text's whole variety.
pub struct SyntheticText {
  blocks: Vec<String>,
  output_lines: Vec<String>,
}

impl SyntheticText {
  pub fn new(rng: &mut Rng) -> Self {
    let blocks = (0..BLOCK_POOL_SIZE).map(|_| compose_block(rng)).collect();
    let output_lines = (0..OUTPUT_LINE_POOL_SIZE).map(|_| compose_output_line(rng)).collect();
    Self { blocks, output_lines }
  }

  /// One assistant block, drawn from the pool.
  pub fn block(&self, rng: &mut Rng) -> &str {
    rng.choose(&self.blocks).as_str()
  }

  /// `lines` tool-output lines, each drawn from the pool with replacement, joined by newlines.
  pub fn tool_output(&self, rng: &mut Rng, lines: u64) -> String {
    let picked: Vec<&str> = (0..lines).map(|_| rng.choose(&self.output_lines).as_str()).collect();
    picked.join("\n")
  }
}

/// The English sentences joined by spaces. The filler message repeats this ASCII text to reach its size.
pub fn filler_source() -> String {
  EN_SENTENCES.join(" ")
}

/// One sentence, in Chinese or English with equal odds.
pub fn phrase(rng: &mut Rng) -> &'static str {
  if rng.unit() < 0.5 { rng.choose(ZH_SENTENCES) } else { rng.choose(EN_SENTENCES) }
}

/// Two to four sentences of one language: Chinese runs together, English is joined by spaces.
pub fn paragraph(rng: &mut Rng) -> String {
  if rng.unit() < 0.5 {
    let count = rng.range(2, 4);
    (0..count).map(|_| *rng.choose(ZH_SENTENCES)).collect()
  } else {
    let count = rng.range(2, 4);
    let sentences: Vec<&str> = (0..count).map(|_| *rng.choose(EN_SENTENCES)).collect();
    sentences.join(" ")
  }
}

/// A session title: a topic and a number, and rarely a long run of sentences.
pub fn title(rng: &mut Rng) -> String {
  let topic = if rng.unit() < 0.5 { *rng.choose(ZH_TOPICS) } else { *rng.choose(EN_TOPICS) };
  let mut parts = vec![topic.to_string(), format!("#{}", rng.range(1, 999))];
  if rng.unit() < 0.015 {
    let count = rng.range(10, 30);
    parts.extend((0..count).map(|_| phrase(rng).to_string()));
  }
  parts.join(" / ")
}

/// A tool call's name and input, shaped as the agent's own tool uses are.
pub fn tool_call(rng: &mut Rng) -> (&'static str, Value) {
  let name = *rng.choose(TOOL_NAMES);
  let module = format!("module_{:02}", rng.range(1, 40));
  let input = match name {
    "Read" => json!({"file_path": format!("src/{module}/handler.rs")}),
    "Bash" => json!({"command": format!("cargo test -p {module} --locked")}),
    "Grep" => json!({"pattern": "TODO|FIXME", "path": format!("src/{module}")}),
    "Edit" => {
      json!({"file_path": format!("src/{module}/config.rs"), "old_string": "retry: 3", "new_string": "retry: 5"})
    }
    "Glob" => json!({"pattern": format!("src/{module}/**/*.rs")}),
    other => unreachable!("tool names are drawn from TOOL_NAMES, not {other}"),
  };
  (name, input)
}

/// An id for one tool call. The same id names a tool use and the result that answers it.
pub fn tool_id(rng: &mut Rng) -> String {
  format!("toolu_{:012x}", rng.next_u64() >> 16)
}

/// One block of one of five shapes: a paragraph, a heading with a list, a table, a code fence or a numbered list.
fn compose_block(rng: &mut Rng) -> String {
  let kind = rng.unit();
  if kind < 0.55 {
    return paragraph(rng);
  }
  if kind < 0.7 {
    let heading = *rng.choose(ZH_TOPICS);
    let count = rng.range(2, 5);
    let items: Vec<String> = (0..count).map(|_| format!("- {}", phrase(rng))).collect();
    return format!("## {heading}\n\n{}", items.join("\n"));
  }
  if kind < 0.8 {
    let topic = *rng.choose(EN_TOPICS);
    let row = format!("| {topic} | 完成 | {} |", phrase(rng));
    return format!("| 项目 | 状态 | 说明 |\n| --- | --- | --- |\n{row}");
  }
  if kind < 0.92 {
    let (language, code) = *rng.choose(CODE_SNIPPETS);
    return format!("```{language}\n{code}\n```");
  }
  let items: Vec<String> = (1..=3)
    .map(|index| {
      let topic = *rng.choose(EN_TOPICS);
      format!("{index}. **{topic}**: {}", phrase(rng))
    })
    .collect();
  items.join("\n")
}

/// One tool-output line: a source location and a sentence.
fn compose_output_line(rng: &mut Rng) -> String {
  let module = format!("module_{:02}", rng.range(1, 40));
  let file = rng.range(1, 300);
  let line = rng.range(1, 900);
  format!("src/{module}/file_{file:03}.rs:{line}: {}", phrase(rng))
}

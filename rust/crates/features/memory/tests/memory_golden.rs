use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use charliebot_memory::memory::{self, MemorySelection};
use flate2::read::GzDecoder;
use serde_json::{Value, json};

#[test]
fn memory_selections_and_store_setup_match_the_recorded_python_cases() {
  let mut decoder = GzDecoder::new(include_bytes!("fixtures/memory_golden.json.gz").as_slice());
  let mut fixture_text = String::new();
  decoder.read_to_string(&mut fixture_text).unwrap();
  let fixture: Value = serde_json::from_str(&fixture_text).unwrap();
  assert_eq!(fixture["base_commit"], "a572e7ca71858d9ba52f0abc2726fa44213210ac");
  assert_eq!(fixture["seed"], 20261018);
  assert_eq!(fixture["random_stores"], 80);
  assert_eq!(fixture["ensure_dirs"], 4);
  assert_eq!(fixture["stores"].as_array().unwrap().len(), 119);

  let root = tempfile::tempdir().unwrap();
  let mut counts = json!({"selections": 0, "segments": 0, "none": 0, "format_errors": 0});
  for (index, store) in fixture["stores"].as_array().unwrap().iter().enumerate() {
    let store_dir =
      root.path().join("stores").join(format!("{index:03}-{}", store["name"].as_str().unwrap())).join("memory");
    materialize(&store_dir, &store["files"]);
    let requests = store["requests"].as_array().unwrap();
    let expected = store["expected"].as_array().unwrap();
    assert_eq!(requests.len(), 6);
    assert_eq!(expected.len(), requests.len());
    for (request, expected) in requests.iter().zip(expected) {
      let answer = match request["audience"].as_str().unwrap() {
        "master" => memory::select_master_memory(&store_dir).map(|selection| selection.map(selection_json)),
        "worker" => memory::select_worker_memory(&store_dir, request["repo_basename"].as_str().unwrap())
          .map(|selection| Some(selection_json(selection))),
        audience => panic!("unknown recorded audience {audience:?}"),
      };
      let actual = match answer {
        Ok(selection) => json!({"selection": selection}),
        Err(error) => json!({"error": error.to_string()}),
      };
      assert_eq!(
        actual,
        materialize_paths(expected.clone(), root.path()),
        "store={} request={request:?}",
        store["name"]
      );
      if actual.get("error").is_some() {
        counts["format_errors"] = counts["format_errors"].as_u64().unwrap().checked_add(1).unwrap().into();
      } else if actual["selection"].is_null() {
        counts["none"] = counts["none"].as_u64().unwrap().checked_add(1).unwrap().into();
      } else {
        counts["selections"] = counts["selections"].as_u64().unwrap().checked_add(1).unwrap().into();
        counts["segments"] = counts["segments"]
          .as_u64()
          .unwrap()
          .checked_add(actual["selection"]["segments"].as_array().unwrap().len() as u64)
          .unwrap()
          .into();
      }
    }
  }
  assert_eq!(counts, fixture["summary"]);

  let ensure_expected = expected_tree(&fixture["ensure_tree"]);
  for index in 0..fixture["ensure_dirs"].as_u64().unwrap() {
    let directory = root.path().join("ensure").join(index.to_string());
    fs::create_dir_all(&directory).unwrap();
    for _ in 0..2 {
      memory::ensure_store(&directory).unwrap();
      assert_eq!(tree(&directory), ensure_expected, "ensure directory {index}");
    }
  }
}

fn selection_json(selection: MemorySelection) -> Value {
  json!({
    "audience": selection.audience,
    "repo_basename": selection.repo_basename,
    "segments": selection.segments.iter().map(|segment| json!({
      "delivery": segment.delivery,
      "text": segment.text,
      "sources": segment.sources.iter().map(|source| json!({
        "source_ref": source.source_ref,
        "delivery": source.delivery,
        "text": source.text,
      })).collect::<Vec<_>>(),
    })).collect::<Vec<_>>(),
    "usage_line": selection.usage_line,
    "text": selection.text,
  })
}

fn materialize(root: &Path, files: &Value) {
  fs::create_dir_all(root).unwrap();
  for (relative, text) in files.as_object().unwrap() {
    let path = root.join(relative);
    match text {
      Value::Null => fs::create_dir_all(path).unwrap(),
      Value::String(text) => {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
      }
      value => panic!("recorded memory file must be text or a directory, got {value:?}"),
    }
  }
}

fn materialize_paths(mut value: Value, root: &Path) -> Value {
  match &mut value {
    Value::String(text) => *text = text.replace("$TMPROOT", &root.to_string_lossy()),
    Value::Array(values) => values.iter_mut().for_each(|value| *value = materialize_paths(value.clone(), root)),
    Value::Object(values) => values.values_mut().for_each(|value| *value = materialize_paths(value.clone(), root)),
    _ => {}
  }
  value
}

fn expected_tree(value: &Value) -> BTreeMap<String, Option<Vec<u8>>> {
  value
    .as_object()
    .unwrap()
    .iter()
    .map(|(path, content)| {
      let content = content.as_str().map(decode_hex);
      (path.clone(), content)
    })
    .collect()
}

fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
  fn visit(root: &Path, directory: &Path, output: &mut BTreeMap<String, Option<Vec<u8>>>) {
    for entry in fs::read_dir(directory).unwrap() {
      let entry = entry.unwrap();
      let path = entry.path();
      let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
      if relative == ".git" || relative.starts_with(".git/") {
        continue;
      }
      if entry.file_type().unwrap().is_dir() {
        output.insert(relative, None);
        visit(root, &path, output);
      } else {
        output.insert(relative, Some(fs::read(path).unwrap()));
      }
    }
  }
  let mut output = BTreeMap::new();
  visit(root, root, &mut output);
  output
}

fn decode_hex(value: &str) -> Vec<u8> {
  assert_eq!(value.len() % 2, 0, "hex fixture has an even number of digits");
  value
    .as_bytes()
    .as_chunks::<2>()
    .0
    .iter()
    .map(|digits| u8::from_str_radix(std::str::from_utf8(digits).unwrap(), 16).unwrap())
    .collect()
}

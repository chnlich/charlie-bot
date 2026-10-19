use std::io::Read;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use charliebot_compat::fastapi_body::{self, BodyModel, Field, FieldType};
use charliebot_compat::responses;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

const FIELDS: &[Field] = &[
  Field::required("name", FieldType::String),
  Field::required("note", FieldType::NullableString),
  Field::required("count", FieldType::NullableInteger),
  Field::required("enabled", FieldType::Boolean),
  Field::required("mode", FieldType::Literal(&["fast", "safe"])),
  Field::required("labels", FieldType::StringList),
];
const MODEL: BodyModel = BodyModel::required(FIELDS);

struct Case {
  name: &'static str,
  item_id: &'static str,
  body: Vec<u8>,
  content_type: Option<&'static str>,
  expected_status: StatusCode,
  compare_body: bool,
}

struct PythonServer {
  child: Child,
}

impl PythonServer {
  async fn start(address: SocketAddr) -> Self {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../../..").canonicalize().expect("the worktree root exists");
    let script = manifest.join("tests/fixtures/fastapi_body_app.py");
    let mut command = Command::new("uv");
    command
      .args(["run", "python"])
      .arg(script)
      .arg("--port")
      .arg(address.port().to_string())
      .current_dir(root)
      .stdout(Stdio::null())
      .stderr(Stdio::piped());
    command.process_group(0);
    let child = command.spawn().expect("uv run starts the FastAPI test app");
    let mut server = Self { child };
    server.wait_ready(address).await;
    server
  }

  async fn wait_ready(&mut self, address: SocketAddr) {
    timeout(Duration::from_secs(10), async {
      loop {
        if let Some(status) = self.child.try_wait().expect("the FastAPI process status is readable") {
          let mut stderr = String::new();
          if let Some(stream) = self.child.stderr.as_mut() {
            stream.read_to_string(&mut stderr).expect("the FastAPI error log is readable");
          }
          panic!("FastAPI test app exited with {status}: {stderr}");
        }
        match TcpStream::connect(address).await {
          Ok(stream) => {
            drop(stream);
            return;
          }
          Err(_) => sleep(Duration::from_millis(50)).await,
        }
      }
    })
    .await
    .expect("FastAPI test app starts within 10 seconds");
  }
}

impl Drop for PythonServer {
  fn drop(&mut self) {
    if self.child.try_wait().expect("the FastAPI process status is readable").is_none() {
      let process_group = format!("-{}", self.child.id());
      let signal = Command::new("/usr/bin/kill")
        .args(["-TERM", "--", process_group.as_str()])
        .status()
        .expect("the FastAPI process group can be stopped");
      assert!(signal.success(), "the FastAPI process group stop succeeded");
    }
    self.child.wait().expect("the FastAPI process is reaped");
  }
}

#[tokio::test]
async fn fastapi_body_matches_fastapi_for_the_request_corpus() {
  rustls::crypto::ring::default_provider().install_default().expect("the test installs its rustls crypto provider");
  let python_reservation = StdTcpListener::bind("127.0.0.1:0").expect("a loopback port is available");
  let python_address = python_reservation.local_addr().expect("the Python address is readable");
  let rust_listener = TcpListener::bind("127.0.0.1:0").await.expect("a second loopback port is available");
  let rust_address = rust_listener.local_addr().expect("the Rust address is readable");
  drop(python_reservation);

  let python = PythonServer::start(python_address).await;
  let rust_server = tokio::spawn(async move {
    axum::serve(rust_listener, Router::new().route("/items/{item_id}", post(read_item)))
      .await
      .expect("the axum test app serves requests");
  });

  let client = Client::new();
  let cases = corpus();
  assert_eq!(cases.len(), 40);
  for case in &cases {
    let (python_status, python_body) = send(&client, python_address, case.item_id, case.content_type, &case.body).await;
    let (rust_status, rust_body) = send(&client, rust_address, case.item_id, case.content_type, &case.body).await;

    assert_eq!(python_status, case.expected_status, "FastAPI status for {}", case.name);
    assert_eq!(rust_status, python_status, "status for {}", case.name);
    if case.compare_body {
      assert_eq!(rust_body, python_body, "body for {}", case.name);
    } else if case.expected_status == StatusCode::UNPROCESSABLE_ENTITY {
      assert_eq!(error_shape(&rust_body), error_shape(&python_body), "validation shape for {}", case.name);
    }
  }

  rust_server.abort();
  match rust_server.await {
    Err(error) if error.is_cancelled() => {}
    Err(error) => panic!("axum test app stopped with {error}"),
    Ok(()) => panic!("axum test app ended before cancellation"),
  }
  drop(python);
}

async fn read_item(request: Request<Body>) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match fastapi_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let item_id = parts.uri.path().rsplit('/').next().expect("the route has an item id");
  if item_id == "missing" {
    return responses::json(StatusCode::NOT_FOUND, &json!({"detail": "missing item"}));
  }
  let body = match body.validate(&MODEL) {
    Ok(Some(body)) => body,
    Ok(None) => unreachable!("the differential model body is required"),
    Err(response) => return *response,
  };
  responses::json(StatusCode::OK, &body)
}

async fn send(
  client: &Client,
  address: SocketAddr,
  item_id: &str,
  content_type: Option<&str>,
  body: &[u8],
) -> (StatusCode, Value) {
  let mut request = client.post(format!("http://{address}/items/{item_id}")).body(body.to_vec());
  if let Some(content_type) = content_type {
    request = request.header(header::CONTENT_TYPE, content_type);
  }
  let response = request.send().await.expect("the server accepts the request");
  let status = response.status();
  let body = response.bytes().await.expect("the response body is readable");
  let body = serde_json::from_slice(&body).expect("the response body is JSON");
  (status, body)
}

fn error_shape(body: &Value) -> Vec<(Value, Value)> {
  body["detail"]
    .as_array()
    .expect("a 422 response has a detail list")
    .iter()
    .map(|error| (error["type"].clone(), error["loc"].clone()))
    .collect()
}

fn corpus() -> Vec<Case> {
  let valid = valid_body();
  let mut extra = model_value();
  extra.as_object_mut().expect("the model value is an object").insert("ignored".to_string(), json!("extra"));
  let mut missing_field = model_value();
  missing_field.as_object_mut().expect("the model value is an object").remove("labels");

  vec![
    Case {
      name: "empty body",
      item_id: "present",
      body: Vec::new(),
      content_type: Some("application/json"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      compare_body: false,
    },
    Case {
      name: "missing content type keeps raw bytes",
      item_id: "present",
      body: valid.clone(),
      content_type: None,
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      compare_body: false,
    },
    Case {
      name: "text content type keeps raw bytes",
      item_id: "present",
      body: valid.clone(),
      content_type: Some("text/plain"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      compare_body: false,
    },
    Case {
      name: "application JSON with charset",
      item_id: "present",
      body: valid.clone(),
      content_type: Some("application/json; charset=utf-8"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    Case {
      name: "vendor JSON media type",
      item_id: "present",
      body: valid.clone(),
      content_type: Some("application/vnd.api+json"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    Case {
      name: "JSON subtype with a space before +json",
      item_id: "present",
      body: valid.clone(),
      content_type: Some("application/x y+json"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    json_error_case("expecting value", b"?"),
    json_error_case("invalid control character", b"\"line\nbreak\""),
    json_error_case("unterminated string", b"\"unfinished"),
    json_error_case("invalid escape", br#""\x""#),
    json_error_case("invalid unicode escape", br#""\u12""#),
    json_error_case("expecting property name", b"{x: 1}"),
    json_error_case("expecting colon", br#"{"name" 1}"#),
    json_error_case("expecting comma", b"[1 2]"),
    json_error_case("trailing object comma", br#"{"name":1,}"#),
    json_error_case("trailing array comma", b"[1,]"),
    json_error_case("extra data", b"{} {}"),
    Case {
      name: "UTF-16 body with BOM",
      item_id: "present",
      body: utf16_le_bom(std::str::from_utf8(&valid).expect("the valid JSON is UTF-8")),
      content_type: Some("application/json; charset=utf-16-le"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    Case {
      name: "UTF-32 body without BOM",
      item_id: "present",
      body: utf32_le(std::str::from_utf8(&valid).expect("the valid JSON is UTF-8")),
      content_type: Some("application/json"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    Case {
      name: "invalid UTF-8 body",
      item_id: "present",
      body: vec![b'{', 0xff, b'}'],
      content_type: Some("application/json"),
      expected_status: StatusCode::BAD_REQUEST,
      compare_body: false,
    },
    json_value_case("JSON array", json!([])),
    json_value_case("JSON string", json!("body")),
    json_value_case("JSON null", Value::Null),
    json_field_case("str field type", "name", Value::Null),
    json_field_case("nullable str field type", "note", json!(7)),
    json_field_case("nullable int field type", "count", json!([])),
    json_field_case("bool field type", "enabled", json!("unknown")),
    json_field_case("Literal field type", "mode", json!("other")),
    json_field_case("list field type", "labels", json!({})),
    json_field_case("list item type", "labels", json!(["one", 2])),
    json_field_case("fractional int field", "count", json!(1.5)),
    json_field_case("float above the i64 range", "count", json!(9.3e18)),
    json_field_case("float at the negative i64 bound", "count", json!(-9223372036854775808.0)),
    Case {
      name: "float just below the i64 bound",
      item_id: "present",
      body: json_bytes(&field_overridden("count", json!(9223372036854774784.0))),
      content_type: Some("application/json"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    Case {
      name: "integer above the i64 range",
      item_id: "present",
      body: json_bytes(&field_overridden("count", json!(9223372036854775808u64))),
      content_type: Some("application/json"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    Case {
      name: "boolean coerces to integer",
      item_id: "present",
      body: json_bytes(&field_overridden("count", json!(true))),
      content_type: Some("application/json"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    json_value_case("missing required field", missing_field),
    Case {
      name: "extra fields are ignored",
      item_id: "present",
      body: json_bytes(&extra),
      content_type: Some("application/json"),
      expected_status: StatusCode::OK,
      compare_body: true,
    },
    Case {
      name: "malformed JSON for a missing id",
      item_id: "missing",
      body: b"{".to_vec(),
      content_type: Some("application/json"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      compare_body: false,
    },
    Case {
      name: "dependency runs before model validation",
      item_id: "missing",
      body: json_bytes(&field_overridden("count", json!([]))),
      content_type: Some("application/json"),
      expected_status: StatusCode::NOT_FOUND,
      compare_body: false,
    },
  ]
}

fn json_error_case(name: &'static str, body: &[u8]) -> Case {
  Case {
    name,
    item_id: "present",
    body: body.to_vec(),
    content_type: Some("application/json"),
    expected_status: StatusCode::UNPROCESSABLE_ENTITY,
    compare_body: false,
  }
}

fn json_value_case(name: &'static str, value: Value) -> Case {
  Case {
    name,
    item_id: "present",
    body: json_bytes(&value),
    content_type: Some("application/json"),
    expected_status: StatusCode::UNPROCESSABLE_ENTITY,
    compare_body: false,
  }
}

fn json_field_case(name: &'static str, field: &str, value: Value) -> Case {
  Case {
    name,
    item_id: "present",
    body: json_bytes(&field_overridden(field, value)),
    content_type: Some("application/json"),
    expected_status: StatusCode::UNPROCESSABLE_ENTITY,
    compare_body: false,
  }
}

fn valid_body() -> Vec<u8> {
  json_bytes(&model_value())
}

fn model_value() -> Value {
  json!({
      "name": "sample",
      "note": null,
      "count": 7,
      "enabled": true,
      "mode": "fast",
      "labels": ["one", "two"],
  })
}

fn field_overridden(field: &str, value: Value) -> Value {
  let mut model = model_value();
  model.as_object_mut().expect("the model value is an object").insert(field.to_string(), value);
  model
}

fn json_bytes(value: &Value) -> Vec<u8> {
  serde_json::to_vec(value).expect("the test JSON serializes")
}

fn utf16_le_bom(text: &str) -> Vec<u8> {
  let mut bytes = vec![0xff, 0xfe];
  for unit in text.encode_utf16() {
    bytes.extend_from_slice(&unit.to_le_bytes());
  }
  bytes
}

fn utf32_le(text: &str) -> Vec<u8> {
  text.chars().flat_map(|character| u32::from(character).to_le_bytes()).collect()
}

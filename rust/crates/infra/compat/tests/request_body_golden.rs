use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use charliebot_compat::request_body::{self, BodyModel, Field, FieldType, TaggedVariant};
use charliebot_compat::responses;
use charliebot_compat::validation::Bound;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const PYTHON_FIXTURE: &str = include_str!("fixtures/request_body_golden.json");

const FIELDS: &[Field] = &[
  Field::required("name", FieldType::String),
  Field::required("note", FieldType::NullableString),
  Field::required("count", FieldType::NullableInteger),
  Field::required("enabled", FieldType::Boolean),
  Field::required("mode", FieldType::Literal(&["fast", "safe"])),
  Field::required("labels", FieldType::StringList),
];
const MODEL: BodyModel = BodyModel::required(FIELDS);
const BOUNDED_FIELDS: &[Field] = &[
  Field::optional("offset", FieldType::NullableInteger).ge(Bound::Int(0)),
  Field::required("count", FieldType::Integer).ge(Bound::Int(1)).le(Bound::Int(100)),
  Field::optional("ratio", FieldType::NullableFloat).gt(Bound::Float(0.0)).le(Bound::Float(1.0)),
  Field::required("label", FieldType::String).min_length(1),
  Field::required("tags", FieldType::StringList).max_length(2),
];
const BOUNDED_MODEL: BodyModel = BodyModel::required(BOUNDED_FIELDS).forbid_extra();
const UPLOADED_FILE_FIELDS: &[Field] = &[
  Field::required("filename", FieldType::String),
  Field::required("path", FieldType::String),
  Field::optional("size", FieldType::NullableInteger),
];
const UPLOADED_FILE: BodyModel = BodyModel::optional(UPLOADED_FILE_FIELDS);
const MESSAGE_FIELDS: &[Field] = &[
  Field::required("content", FieldType::String),
  Field::optional("uploaded_files", FieldType::ModelList { class_name: "UploadedFileRef", model: &UPLOADED_FILE }),
  Field::optional("input_mode", FieldType::NullableString),
];
const MESSAGE: BodyModel = BodyModel::required(MESSAGE_FIELDS);
const ATTACHMENT_FIELDS: &[Field] =
  &[Field::required("filename", FieldType::String), Field::optional("size", FieldType::NullableInteger)];
const ATTACHMENT: BodyModel = BodyModel::optional(ATTACHMENT_FIELDS).forbid_extra();
const ATTACHMENTS_FIELDS: &[Field] =
  &[Field::optional("attachments", FieldType::NullableModelList { class_name: "StrictFileRef", model: &ATTACHMENT })
    .min_length(2)
    .max_length(3)];
const ATTACHMENTS: BodyModel = BodyModel::required(ATTACHMENTS_FIELDS);
const LOCAL_PID_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&["local_pid"])),
  Field::required("pid", FieldType::Integer),
  Field::optional("exit_file", FieldType::NullableString),
  Field::optional("log", FieldType::NullableString),
];
const LOCAL_PID: BodyModel = BodyModel::optional(LOCAL_PID_FIELDS);
const REMOTE_PID_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&["remote_pid"])),
  Field::required("host", FieldType::String),
  Field::required("pid", FieldType::Integer),
  Field::optional("exit_file", FieldType::NullableString),
  Field::optional("log", FieldType::NullableString),
];
const REMOTE_PID: BodyModel = BodyModel::optional(REMOTE_PID_FIELDS);
const SLURM_JOB_FIELDS: &[Field] = &[
  Field::required("kind", FieldType::Literal(&["slurm_job"])),
  Field::optional("host", FieldType::NullableString),
  Field::required("job_id", FieldType::Integer),
];
const SLURM_JOB: BodyModel = BodyModel::optional(SLURM_JOB_FIELDS);
const WATCH_TARGETS: &[TaggedVariant] = &[
  TaggedVariant { tag: "local_pid", class_name: "LocalPid", model: &LOCAL_PID },
  TaggedVariant { tag: "remote_pid", class_name: "RemotePid", model: &REMOTE_PID },
  TaggedVariant { tag: "slurm_job", class_name: "SlurmJob", model: &SLURM_JOB },
];
const TRIGGER_FIELDS: &[Field] = &[
  Field::required("session_id", FieldType::String),
  Field::required("delay_seconds", FieldType::Integer),
  Field::required("message", FieldType::String).max_length(200),
  Field::optional(
    "watch_targets",
    FieldType::NullableTaggedUnionList { discriminator: "kind", variants: WATCH_TARGETS },
  ),
];
const TRIGGER: BodyModel = BodyModel::required(TRIGGER_FIELDS).forbid_extra();

struct Case {
  name: &'static str,
  item_id: &'static str,
  body: Vec<u8>,
  content_type: Option<&'static str>,
  expected_status: StatusCode,
  compare_body: bool,
}

#[tokio::test]
async fn fastapi_body_matches_fastapi_for_the_request_corpus() {
  rustls::crypto::ring::default_provider().install_default().expect("the test installs its rustls crypto provider");
  let rust_listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port is available");
  let rust_address = rust_listener.local_addr().expect("the Rust address is readable");
  let rust_server = tokio::spawn(async move {
    let router = Router::new()
      .route("/items/{item_id}", post(read_item))
      .route("/bounds/{item_id}", post(read_bounded))
      .route("/messages/{item_id}", post(read_message))
      .route("/attachments/{item_id}", post(read_attachments))
      .route("/triggers/{item_id}", post(read_trigger));
    axum::serve(rust_listener, router).await.expect("the axum test app serves requests");
  });

  let client = Client::new();
  let fixture: Value = serde_json::from_str(PYTHON_FIXTURE).unwrap();
  assert_eq!(fixture["source"], "rust/crates/infra/compat/tests/fixtures/fastapi_body_app.py");
  assert_eq!(fixture["base_commit"], "d4021ce3993d7e36b2709f00caf578e1f8541407");
  let fixture_cases = fixture["cases"].as_array().unwrap();
  assert_eq!(fixture_cases.len(), 137);
  let mut fixture_index = 0;
  let items = corpus();
  assert_eq!(items.len(), 40);
  compare_corpus(&client, rust_address, "/items", &items, fixture_cases, &mut fixture_index).await;
  let bounded = bounded_corpus();
  assert_eq!(bounded.len(), 28);
  compare_corpus(&client, rust_address, "/bounds", &bounded, fixture_cases, &mut fixture_index).await;
  let messages = message_corpus();
  assert_eq!(messages.len(), 17);
  compare_corpus(&client, rust_address, "/messages", &messages, fixture_cases, &mut fixture_index).await;
  let attachments = attachment_corpus();
  assert_eq!(attachments.len(), 20);
  compare_corpus(&client, rust_address, "/attachments", &attachments, fixture_cases, &mut fixture_index).await;
  let triggers = trigger_corpus();
  assert_eq!(triggers.len(), 32);
  compare_corpus(&client, rust_address, "/triggers", &triggers, fixture_cases, &mut fixture_index).await;
  assert_eq!(fixture_index, fixture_cases.len());

  rust_server.abort();
  match rust_server.await {
    Err(error) if error.is_cancelled() => {}
    Err(error) => panic!("axum test app stopped with {error}"),
    Ok(()) => panic!("axum test app ended before cancellation"),
  }
}

async fn read_item(request: Request<Body>) -> Response {
  read_model(request, &MODEL).await
}

async fn read_bounded(request: Request<Body>) -> Response {
  read_model(request, &BOUNDED_MODEL).await
}

async fn read_message(request: Request<Body>) -> Response {
  read_model(request, &MESSAGE).await
}

async fn read_attachments(request: Request<Body>) -> Response {
  read_model(request, &ATTACHMENTS).await
}

async fn read_trigger(request: Request<Body>) -> Response {
  read_model(request, &TRIGGER).await
}

async fn read_model(request: Request<Body>, model: &BodyModel) -> Response {
  let (parts, stream) = request.into_parts();
  let body = match request_body::read(&parts.headers, stream).await {
    Ok(body) => body,
    Err(response) => return *response,
  };
  let item_id = parts.uri.path().rsplit('/').next().expect("the route has an item id");
  if item_id == "missing" {
    return responses::json(StatusCode::NOT_FOUND, &json!({"detail": "missing item"}));
  }
  let body = match body.validate(model) {
    Ok(Some(body)) => body,
    Ok(None) => unreachable!("the differential model body is required"),
    Err(response) => return *response,
  };
  responses::json(StatusCode::OK, &body)
}

async fn compare_corpus(
  client: &Client,
  rust_address: SocketAddr,
  route: &str,
  cases: &[Case],
  fixture_cases: &[Value],
  fixture_index: &mut usize,
) {
  for case in cases {
    let recorded = &fixture_cases[*fixture_index];
    let request = json!({
      "path": format!("{route}/{}", case.item_id),
      "name": case.name,
      "content_type": case.content_type,
      "body_hex": hex_bytes(&case.body),
    });
    assert_eq!(recorded["request"], request, "recorded request for {}", case.name);
    let status = u16::try_from(recorded["expected"]["status"].as_u64().unwrap()).unwrap();
    let recorded_status = StatusCode::from_u16(status).unwrap();
    let recorded_body = &recorded["expected"]["body"];
    let (rust_status, rust_body) = send(client, rust_address, route, case.item_id, case.content_type, &case.body).await;

    assert_eq!(recorded_status, case.expected_status, "FastAPI status for {}", case.name);
    assert_eq!(rust_status, recorded_status, "status for {}", case.name);
    if case.compare_body {
      assert_eq!(&rust_body, recorded_body, "body for {}", case.name);
    } else if recorded_status == StatusCode::UNPROCESSABLE_ENTITY {
      assert_eq!(error_shape(&rust_body), error_shape(recorded_body), "validation shape for {}", case.name);
    }
    *fixture_index += 1;
  }
}

fn hex_bytes(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn send(
  client: &Client,
  address: SocketAddr,
  route: &str,
  item_id: &str,
  content_type: Option<&str>,
  body: &[u8],
) -> (StatusCode, Value) {
  let mut request = client.post(format!("http://{address}{route}/{item_id}")).body(body.to_vec());
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

/// The bounded model's corpus, sent to `/bounds`: each bound at, below and above its limit, null on each optional
/// field, and two extra keys. One case holds a missing field, a wrong type and an extra key together, to check their
/// error order.
fn bounded_corpus() -> Vec<Case> {
  vec![
    json_ok_case("the valid body", bounded_value()),
    json_ok_case("offset at its lower bound", bounded_with(&[("offset", json!(0))])),
    json_value_case("offset below its lower bound", bounded_with(&[("offset", json!(-1))])),
    json_ok_case("offset null", bounded_with(&[("offset", Value::Null)])),
    json_ok_case("count at its lower bound", bounded_with(&[("count", json!(1))])),
    json_value_case("count below its lower bound", bounded_with(&[("count", json!(0))])),
    json_ok_case("count at its upper bound", bounded_with(&[("count", json!(100))])),
    json_value_case("count above its upper bound", bounded_with(&[("count", json!(101))])),
    json_ok_case("ratio at its upper bound as an integer", bounded_with(&[("ratio", json!(1))])),
    json_value_case("ratio above its upper bound", bounded_with(&[("ratio", json!(1.5))])),
    json_value_case("ratio at its lower bound", bounded_with(&[("ratio", json!(0.0))])),
    json_value_case("ratio below its lower bound", bounded_with(&[("ratio", json!(-0.5))])),
    json_ok_case("ratio null", bounded_with(&[("ratio", Value::Null)])),
    json_ok_case("ratio as a numeric string", bounded_with(&[("ratio", json!("0.5"))])),
    json_ok_case("ratio as a boolean", bounded_with(&[("ratio", json!(true))])),
    json_value_case("ratio as a word", bounded_with(&[("ratio", json!("high"))])),
    json_ok_case("label at its minimum length", bounded_with(&[("label", json!("a"))])),
    json_value_case("label below its minimum length", bounded_with(&[("label", json!(""))])),
    json_ok_case("label above its minimum length", bounded_with(&[("label", json!("ab"))])),
    json_ok_case("tags at its maximum length", bounded_with(&[("tags", json!(["x", "y"]))])),
    json_ok_case("tags empty", bounded_with(&[("tags", json!([]))])),
    json_value_case("tags above its maximum length", bounded_with(&[("tags", json!(["x", "y", "z"]))])),
    json_value_case("tags over the maximum with a wrong item", bounded_with(&[("tags", json!(["x", 3, "z"]))])),
    json_value_case("tags with a wrong item within the maximum", bounded_with(&[("tags", json!(["x", 3]))])),
    json_value_case("two extra keys in input order", bounded_with(&[("zeta", json!(1)), ("alpha", json!(2))])),
    json_value_case(
      "a missing field, a wrong type and an extra key",
      json!({"offset": 3, "ratio": 0.5, "label": 5, "tags": ["x"], "colour": "red"}),
    ),
    json_value_case("a bound error and an extra key", bounded_with(&[("ratio", json!(2.0)), ("extra", json!(1))])),
    json_value_case(
      "bound errors in declaration order",
      bounded_with(&[("count", json!(0)), ("label", json!("")), ("tags", json!(["x", "y", "z"]))]),
    ),
  ]
}

/// The bounded model's valid body: every bound holds, so a case moves one field out of its bound.
fn bounded_value() -> Value {
  json!({"offset": 3, "count": 50, "ratio": 0.5, "label": "a", "tags": ["x"]})
}

/// The valid bounded body with each `(field, value)` set. A name the model does not declare is an extra key, in the
/// order given.
fn bounded_with(changes: &[(&str, Value)]) -> Value {
  with_changes(bounded_value(), changes)
}

/// The message model's corpus, sent to `/messages`: the valid body, each non-list value and each kind of bad item for
/// `uploaded_files`. Two more cases carry a wrong `content` with a bad item, and two bad items with two errors each.
fn message_corpus() -> Vec<Case> {
  vec![
    json_ok_case("the valid message", message_value()),
    json_ok_case("uploaded_files absent keeps the default", json!({"content": "hi"})),
    json_ok_case("uploaded_files empty", message_with(&[("uploaded_files", json!([]))])),
    json_ok_case("input_mode null", message_with(&[("input_mode", Value::Null)])),
    json_ok_case(
      "an extra key in an uploaded item is ignored",
      message_with(&[("uploaded_files", json!([{"filename": "a.txt", "path": "uploads/a.txt", "colour": "red"}]))]),
    ),
    json_value_case("uploaded_files null", message_with(&[("uploaded_files", Value::Null)])),
    json_value_case("uploaded_files as a string", message_with(&[("uploaded_files", json!("a.txt"))])),
    json_value_case(
      "uploaded_files as an object",
      message_with(&[("uploaded_files", json!({"filename": "a.txt", "path": "uploads/a.txt"}))]),
    ),
    json_value_case("uploaded item is a string", message_with(&[("uploaded_files", json!(["a.txt"]))])),
    json_value_case("uploaded item is a number", message_with(&[("uploaded_files", json!([7]))])),
    json_value_case(
      "uploaded item is a list",
      message_with(&[("uploaded_files", json!([["a.txt", "uploads/a.txt"]]))]),
    ),
    json_value_case("uploaded item is null", message_with(&[("uploaded_files", json!([null]))])),
    json_value_case(
      "uploaded item missing its path",
      message_with(&[("uploaded_files", json!([{"filename": "a.txt"}]))]),
    ),
    json_value_case(
      "a wrong content and a bad uploaded item",
      message_with(&[("content", json!(5)), ("uploaded_files", json!([{"filename": 3, "path": "uploads/a.txt"}]))]),
    ),
    json_value_case(
      "two bad uploaded items with two errors each",
      message_with(&[("uploaded_files", json!([{"filename": 1, "path": 2}, {"filename": "b.txt", "size": "big"}]))]),
    ),
    json_value_case("input_mode as a number", message_with(&[("input_mode", json!(5))])),
    json_value_case("missing content", json!({"uploaded_files": []})),
  ]
}

/// The message model's valid body: one uploaded file, with every field the model declares.
fn message_value() -> Value {
  json!({
    "content": "hi",
    "uploaded_files": [{"filename": "a.txt", "path": "uploads/a.txt", "size": 3}],
    "input_mode": "text",
  })
}

fn message_with(changes: &[(&str, Value)]) -> Value {
  with_changes(message_value(), changes)
}

/// The list model's corpus, sent to `/attachments`: the length bounds at, below and above their limits, and a null or
/// absent list. It also carries each non-list value, each kind of bad item, extra keys in an item, and the error order
/// of items with a length error.
fn attachment_corpus() -> Vec<Case> {
  vec![
    json_ok_case("the valid attachments", attachments_value()),
    json_ok_case("attachments absent keeps the default", json!({})),
    json_ok_case("attachments null", json!({"attachments": null})),
    json_ok_case(
      "attachments at the maximum length",
      attachments_with(&[("attachments", json!([{"filename": "a"}, {"filename": "b"}, {"filename": "c", "size": 1}]))]),
    ),
    json_value_case(
      "attachments below the minimum length",
      attachments_with(&[("attachments", json!([{"filename": "a.txt"}]))]),
    ),
    json_value_case("attachments empty", attachments_with(&[("attachments", json!([]))])),
    json_value_case(
      "attachments above the maximum length",
      attachments_with(&[(
        "attachments",
        json!([{"filename": "a"}, {"filename": "b"}, {"filename": "c"}, {"filename": "d"}]),
      )]),
    ),
    json_value_case(
      "too long with bad items reports only the length",
      attachments_with(&[("attachments", json!([{"filename": 1}, "x", 3, [], {"filename": "e"}]))]),
    ),
    json_value_case(
      "too short with a bad item reports only the item",
      attachments_with(&[("attachments", json!([{"filename": 1}]))]),
    ),
    json_value_case("attachments as a string", attachments_with(&[("attachments", json!("a.txt"))])),
    json_value_case("attachments as an object", attachments_with(&[("attachments", json!({"filename": "a.txt"}))])),
    json_value_case(
      "attachment is a string",
      attachments_with(&[("attachments", json!(["a.txt", {"filename": "b"}]))]),
    ),
    json_value_case("attachment is a number", attachments_with(&[("attachments", json!([7, {"filename": "b"}]))])),
    json_value_case(
      "attachment is a list",
      attachments_with(&[("attachments", json!([["a.txt"], {"filename": "b"}]))]),
    ),
    json_value_case(
      "attachment with an extra key",
      attachments_with(&[("attachments", json!([{"filename": "a.txt", "zzz": 1}, {"filename": "b"}]))]),
    ),
    json_value_case(
      "attachment with two extra keys in input order",
      attachments_with(&[("attachments", json!([{"zeta": 1, "filename": "a.txt", "alpha": 2}, {"filename": "b"}]))]),
    ),
    json_value_case(
      "a field error before an extra key",
      attachments_with(&[("attachments", json!([{"filename": 1, "zzz": 2}, {"filename": "b"}]))]),
    ),
    json_value_case(
      "two bad attachments with two errors each",
      attachments_with(&[("attachments", json!([{"filename": 1, "size": "x"}, {"size": 1.5}]))]),
    ),
    json_value_case(
      "bad attachments in index order",
      attachments_with(&[("attachments", json!([{"filename": 1}, "x", {"filename": "ok", "zzz": 1}]))]),
    ),
    json_value_case(
      "a missing filename",
      attachments_with(&[("attachments", json!([{"size": 1}, {"filename": "b"}]))]),
    ),
  ]
}

/// The list model's valid body: two attachments, the first without a size.
fn attachments_value() -> Value {
  json!({"attachments": [{"filename": "a.txt"}, {"filename": "b.txt", "size": 3}]})
}

fn attachments_with(changes: &[(&str, Value)]) -> Value {
  with_changes(attachments_value(), changes)
}

/// The trigger model's corpus, sent to `/triggers`: one valid item of each variant, null and empty lists, each non-list
/// value and each kind of bad item. It also carries the tag errors (missing, unknown, not a string) and a member's own
/// field errors. The last cases hold a wrong message, and a second bad item beside another error.
fn trigger_corpus() -> Vec<Case> {
  let long_message = "x".repeat(201);
  vec![
    json_ok_case("the valid trigger without watch targets", trigger_value()),
    json_ok_case("a local_pid item", trigger_with(&[("watch_targets", json!([{"kind": "local_pid", "pid": 10}]))])),
    json_ok_case(
      "a remote_pid item with its optional fields",
      trigger_with(&[(
        "watch_targets",
        json!([{"kind": "remote_pid", "host": "h1", "pid": 11, "exit_file": "/x", "log": "/y"}]),
      )]),
    ),
    json_ok_case(
      "a slurm_job item with a null host",
      trigger_with(&[("watch_targets", json!([{"kind": "slurm_job", "host": null, "job_id": 12}]))]),
    ),
    json_ok_case(
      "a slurm_job item without a host",
      trigger_with(&[("watch_targets", json!([{"kind": "slurm_job", "job_id": 12}]))]),
    ),
    json_ok_case(
      "one item of each variant",
      trigger_with(&[(
        "watch_targets",
        json!([
          {"kind": "local_pid", "pid": 1},
          {"kind": "remote_pid", "host": "h1", "pid": 2},
          {"kind": "slurm_job", "job_id": 3},
        ]),
      )]),
    ),
    json_ok_case("watch_targets null", trigger_with(&[("watch_targets", Value::Null)])),
    json_ok_case("watch_targets empty", trigger_with(&[("watch_targets", json!([]))])),
    json_ok_case(
      "an extra key in an item is ignored",
      trigger_with(&[("watch_targets", json!([{"kind": "local_pid", "pid": 1, "zzz": 2}]))]),
    ),
    json_value_case("watch_targets as a string", trigger_with(&[("watch_targets", json!("x"))])),
    json_value_case("watch_targets as a number", trigger_with(&[("watch_targets", json!(5))])),
    json_value_case("watch_targets as an object", trigger_with(&[("watch_targets", json!({"kind": "local_pid"}))])),
    json_value_case("an item missing its kind", trigger_with(&[("watch_targets", json!([{"pid": 10}]))])),
    json_value_case("an item with an unknown kind", trigger_with(&[("watch_targets", json!([{"kind": "pidfile"}]))])),
    json_value_case("an item with kind unknown", trigger_with(&[("watch_targets", json!([{"kind": "unknown"}]))])),
    json_value_case("an item with kind as a number", trigger_with(&[("watch_targets", json!([{"kind": 5}]))])),
    json_value_case("an item with kind null", trigger_with(&[("watch_targets", json!([{"kind": null}]))])),
    json_value_case(
      "a local_pid with a string pid",
      trigger_with(&[("watch_targets", json!([{"kind": "local_pid", "pid": "x"}]))]),
    ),
    json_value_case("a local_pid without a pid", trigger_with(&[("watch_targets", json!([{"kind": "local_pid"}]))])),
    json_value_case(
      "a remote_pid with a number host",
      trigger_with(&[("watch_targets", json!([{"kind": "remote_pid", "host": 3, "pid": 1}]))]),
    ),
    json_value_case(
      "a remote_pid with neither host nor pid",
      trigger_with(&[("watch_targets", json!([{"kind": "remote_pid"}]))]),
    ),
    json_value_case(
      "a slurm_job with a fractional job_id",
      trigger_with(&[("watch_targets", json!([{"kind": "slurm_job", "job_id": 1.5}]))]),
    ),
    json_value_case(
      "a local_pid with two field errors",
      trigger_with(&[("watch_targets", json!([{"kind": "local_pid", "pid": "x", "log": 3}]))]),
    ),
    json_value_case("an item that is a string", trigger_with(&[("watch_targets", json!(["abc"]))])),
    json_value_case("an item that is a number", trigger_with(&[("watch_targets", json!([7]))])),
    json_value_case("an item that is a list", trigger_with(&[("watch_targets", json!([["local_pid", 1]]))])),
    json_value_case("an item that is null", trigger_with(&[("watch_targets", json!([null]))])),
    json_value_case(
      "a wrong message with a bad item",
      trigger_with(&[("message", json!(5)), ("watch_targets", json!([{"pid": 1}]))]),
    ),
    json_value_case("a message over its maximum length", trigger_with(&[("message", json!(long_message))])),
    json_value_case(
      "two bad items",
      trigger_with(&[("watch_targets", json!([{"kind": "pidfile"}, {"kind": "local_pid"}]))]),
    ),
    json_value_case(
      "a valid item beside a slurm_job without its job_id",
      trigger_with(&[("watch_targets", json!([{"kind": "local_pid", "pid": 1}, {"kind": "slurm_job"}]))]),
    ),
    json_value_case(
      "an extra key after a bad item",
      with_changes(trigger_value(), &[("watch_targets", json!([{"pid": 1}])), ("zzz", json!(1))]),
    ),
  ]
}

/// The trigger model's valid body without `watch_targets`: the three required fields.
fn trigger_value() -> Value {
  json!({"session_id": "s1", "delay_seconds": 5, "message": "hi"})
}

fn trigger_with(changes: &[(&str, Value)]) -> Value {
  with_changes(trigger_value(), changes)
}

/// `body` with each `(field, value)` set, in the order given. A name the body does not declare is an extra key.
fn with_changes(mut body: Value, changes: &[(&str, Value)]) -> Value {
  let object = body.as_object_mut().expect("the model value is an object");
  for (field, value) in changes {
    object.insert(field.to_string(), value.clone());
  }
  body
}

fn json_ok_case(name: &'static str, value: Value) -> Case {
  Case {
    name,
    item_id: "present",
    body: json_bytes(&value),
    content_type: Some("application/json"),
    expected_status: StatusCode::OK,
    compare_body: true,
  }
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

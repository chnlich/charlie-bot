use std::io::Read;
use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use charliebot_compat::form_body::{self, Field, FormModel};
use charliebot_compat::responses;
use flate2::read::GzDecoder;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const BOUNDARY: &str = "synthetic-form-boundary";
const PYTHON_FIXTURE: &[u8] = include_bytes!("fixtures/form_body_golden.json.gz");
const FIELDS: &[Field] = &[Field::file("file"), Field::form("note")];
const MODEL: FormModel = FormModel::new(FIELDS);

struct Case {
  name: &'static str,
  item_id: &'static str,
  body: Vec<u8>,
  content_type: String,
  cookie: Option<String>,
  expected_status: StatusCode,
  compare_body: bool,
}

#[tokio::test]
async fn fastapi_form_matches_fastapi_for_the_multipart_corpus() {
  rustls::crypto::ring::default_provider().install_default().expect("the test installs its rustls crypto provider");
  let rust_listener = TcpListener::bind("127.0.0.1:0").await.expect("a second loopback port is available");
  let rust_address = rust_listener.local_addr().expect("the Rust address is readable");
  let rust_server = tokio::spawn(async move {
    axum::serve(rust_listener, Router::new().route("/items/{item_id}", post(read_item)))
      .await
      .expect("the axum test app serves requests");
  });

  let client = Client::new();
  let cases = corpus();
  assert_eq!(cases.len(), 32);
  let mut decoder = GzDecoder::new(PYTHON_FIXTURE);
  let mut fixture_json = String::new();
  decoder.read_to_string(&mut fixture_json).unwrap();
  let fixture: Value = serde_json::from_str(&fixture_json).unwrap();
  assert_eq!(fixture["source"], "rust/crates/infra/compat/tests/fixtures/fastapi_form_app.py");
  assert_eq!(fixture["base_commit"], "d4021ce3993d7e36b2709f00caf578e1f8541407");
  let fixture_cases = fixture["cases"].as_array().unwrap();
  assert_eq!(fixture_cases.len(), cases.len());
  for (case, recorded) in cases.iter().zip(fixture_cases) {
    let request = json!({
      "path": format!("/items/{}", case.item_id),
      "name": case.name,
      "content_type": case.content_type,
      "cookie": case.cookie,
      "body_hex": hex_bytes(&case.body),
    });
    assert_eq!(recorded["request"], request, "recorded request for {}", case.name);
    let status = u16::try_from(recorded["expected"]["status"].as_u64().unwrap()).unwrap();
    let recorded_status = StatusCode::from_u16(status).unwrap();
    let recorded_body = recorded["expected"]["body_text"].as_str().unwrap().as_bytes();
    let (rust_status, rust_body) = send(&client, rust_address, case).await;

    assert_eq!(recorded_status, case.expected_status, "FastAPI status for {}", case.name);
    assert_eq!(rust_status, recorded_status, "status for {}", case.name);
    if case.compare_body {
      assert_eq!(rust_body.as_slice(), recorded_body, "body for {}", case.name);
    } else if recorded_status == StatusCode::UNPROCESSABLE_ENTITY {
      assert_eq!(error_shape(&rust_body), error_shape(recorded_body), "validation shape for {}", case.name);
    }
  }

  rust_server.abort();
  match rust_server.await {
    Err(error) if error.is_cancelled() => {}
    Err(error) => panic!("axum test app stopped with {error}"),
    Ok(()) => panic!("axum test app ended before cancellation"),
  }
}

async fn read_item(request: Request<Body>) -> Response {
  let (parts, body) = request.into_parts();
  let form = match form_body::read(&parts.headers, body).await {
    Ok(form) => form,
    Err(response) => return *response,
  };
  let item_id = parts.uri.path().rsplit('/').next().expect("the route has an item id");
  if item_id == "missing" {
    return responses::json(StatusCode::NOT_FOUND, &json!({"detail": "missing item"}));
  }
  if let Err(response) = form.validate(&MODEL) {
    return *response;
  }
  let file = form.file("file").expect("the required file field validated");
  let note = form.form("note").expect("the required form field validated");
  responses::json(StatusCode::OK, &json!({"filename": file.filename(), "size": file.size(), "note": note}))
}

async fn send(client: &Client, address: SocketAddr, case: &Case) -> (StatusCode, Vec<u8>) {
  let mut request = client
    .post(format!("http://{address}/items/{}", case.item_id))
    .header(header::CONTENT_TYPE, &case.content_type)
    .body(case.body.clone());
  if let Some(cookie) = &case.cookie {
    request = request.header(header::COOKIE, cookie);
  }
  let response = request.send().await.expect("the server accepts the request");
  let status = response.status();
  let body = response.bytes().await.expect("the response body is readable").to_vec();
  (status, body)
}

fn error_shape(body: &[u8]) -> Vec<(Value, Value)> {
  serde_json::from_slice::<Value>(body).expect("a 422 response is JSON")["detail"]
    .as_array()
    .expect("a 422 response has a detail list")
    .iter()
    .map(|error| (error["type"].clone(), error["loc"].clone()))
    .collect()
}

fn hex_bytes(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn corpus() -> Vec<Case> {
  let mut oversized_content_type = format!("multipart/form-data; boundary={}", "x".repeat(20 * 1024));
  let oversized_body = multipart_body(vec![file_part("file", "synthetic.bin", b"x"), form_part("note", b"n")]);
  let mut truncated = format!(
    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"truncated.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
  )
  .into_bytes();
  truncated.extend_from_slice(b"partial file content");

  vec![
    Case {
      name: "valid upload",
      item_id: "present",
      body: multipart_body(vec![
        file_part("file", "synthetic.bin", b"file data"),
        form_part("note", b"synthetic note"),
      ]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
    Case {
      name: "missing file field",
      item_id: "present",
      body: multipart_body(vec![form_part("note", b"synthetic note")]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "file part without filename",
      item_id: "present",
      body: multipart_body(vec![form_part("file", b"plain field"), form_part("note", b"synthetic note")]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "filename with a directory part",
      item_id: "present",
      body: multipart_body(vec![
        file_part("file", "../directory/synthetic.bin", b"path data"),
        form_part("note", b"synthetic note"),
      ]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
    Case {
      name: "empty filename",
      item_id: "present",
      body: multipart_body(vec![file_part("file", "", b"empty name"), form_part("note", b"synthetic note")]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
    Case {
      name: "three mebibyte file",
      item_id: "present",
      body: multipart_body(vec![
        file_part("file", "large.bin", &vec![b'x'; 3 * 1024 * 1024]),
        form_part("note", b"synthetic note"),
      ]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
    Case {
      name: "two files in one field",
      item_id: "present",
      body: multipart_body(vec![
        file_part("file", "first.bin", b"first"),
        file_part("file", "last.bin", b"last file"),
        form_part("note", b"synthetic note"),
      ]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
    Case {
      name: "multipart content type without boundary",
      item_id: "present",
      body: b"multipart body".to_vec(),
      content_type: "multipart/form-data".to_string(),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "truncated multipart body",
      item_id: "present",
      body: truncated,
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "wrong content type",
      item_id: "present",
      body: multipart_body(vec![
        file_part("file", "synthetic.bin", b"file data"),
        form_part("note", b"synthetic note"),
      ]),
      content_type: "application/octet-stream".to_string(),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "boundary the body does not open with",
      item_id: "present",
      body: oversized_body,
      content_type: std::mem::take(&mut oversized_content_type),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "valid upload with a 20 KiB cookie header",
      item_id: "present",
      body: valid_body(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: Some(format!("a={}", "x".repeat(20 * 1024))),
      compare_body: true,
    },
    Case {
      name: "text before the opening boundary",
      item_id: "present",
      body: [b"preamble\r\n".to_vec(), valid_body()].concat(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "CRLF before the opening boundary",
      item_id: "present",
      body: [b"\r\n".to_vec(), valid_body()].concat(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
    Case {
      name: "malformed body for a missing item",
      item_id: "missing",
      body: b"malformed multipart body".to_vec(),
      content_type: "multipart/form-data".to_string(),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "urlencoded body with an empty file field",
      item_id: "present",
      body: b"file=".to_vec(),
      content_type: "application/x-www-form-urlencoded".to_string(),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "urlencoded body with a file value and a note",
      item_id: "present",
      body: b"file=x&note=y".to_vec(),
      content_type: "application/x-www-form-urlencoded".to_string(),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "part header with an invalid character",
      item_id: "present",
      body: format!("--{BOUNDARY}\r\nBad Header: x\r\n\r\nv\r\n--{BOUNDARY}--\r\n").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "part header without a colon",
      item_id: "present",
      body: format!("--{BOUNDARY}\r\nContent-Disposition\r\n\r\nv\r\n--{BOUNDARY}--\r\n").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "part without a closing boundary",
      item_id: "present",
      body: [file_part("file", "open.bin", b"open data"), form_part("note", b"open note")].concat(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "opening boundary without a following CRLF",
      item_id: "present",
      body: format!("--{BOUNDARY}Note: x\r\n\r\nvalue\r\n--{BOUNDARY}--\r\n").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "opening boundary with a CR and no LF",
      item_id: "present",
      body: format!("--{BOUNDARY}\rX\r\n\r\nvalue\r\n--{BOUNDARY}--\r\n").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "closing boundary with one hyphen",
      item_id: "present",
      body: format!("--{BOUNDARY}-X").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "part header with an empty name",
      item_id: "present",
      body: format!("--{BOUNDARY}\r\n: x\r\n\r\nv\r\n--{BOUNDARY}--\r\n").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "part header value ending in a CR without a LF",
      item_id: "present",
      body: format!("--{BOUNDARY}\r\nName: x\rX\r\n\r\nv\r\n--{BOUNDARY}--\r\n").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "part headers ending in a CR without a LF",
      item_id: "present",
      body: format!("--{BOUNDARY}\r\nName: x\r\n\rX").into_bytes(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::BAD_REQUEST,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "boundary text without a CRLF inside a part is data",
      item_id: "present",
      body: [file_part("file", "open.bin", b"open data"), format!("--{BOUNDARY}Note: x\r\n\r\nvalue\r\n").into_bytes()]
        .concat(),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "urlencoded body with a semicolon before a file value",
      item_id: "present",
      body: b"note=a;file=x".to_vec(),
      content_type: "application/x-www-form-urlencoded".to_string(),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "form field with an empty value",
      item_id: "present",
      body: multipart_body(vec![file_part("file", "synthetic.bin", b"file data"), form_part("note", b"")]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "file part with a semicolon in its quoted filename",
      item_id: "present",
      body: multipart_body(vec![file_part("file", "a; b.bin", b"file data"), form_part("note", b"synthetic note")]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
    Case {
      name: "multipart media type with capital letters and a boundary",
      item_id: "present",
      body: valid_body(),
      content_type: format!("Multipart/Form-Data; boundary={BOUNDARY}"),
      expected_status: StatusCode::UNPROCESSABLE_ENTITY,
      cookie: None,
      compare_body: false,
    },
    Case {
      name: "form field with a latin-1 charset",
      item_id: "present",
      body: multipart_body(vec![file_part("file", "synthetic.bin", b"file data"), form_part("note", b"caf\xe9")]),
      content_type: format!("multipart/form-data; boundary={BOUNDARY}; charset=latin-1"),
      expected_status: StatusCode::OK,
      cookie: None,
      compare_body: true,
    },
  ]
}

fn valid_body() -> Vec<u8> {
  multipart_body(vec![file_part("file", "synthetic.bin", b"file data"), form_part("note", b"synthetic note")])
}

fn multipart_body(parts: Vec<Vec<u8>>) -> Vec<u8> {
  let mut body = Vec::new();
  for part in parts {
    body.extend_from_slice(&part);
  }
  body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
  body
}

fn file_part(name: &str, filename: &str, content: &[u8]) -> Vec<u8> {
  let mut part = format!(
    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
  )
  .into_bytes();
  part.extend_from_slice(content);
  part.extend_from_slice(b"\r\n");
  part
}

fn form_part(name: &str, content: &[u8]) -> Vec<u8> {
  let mut part = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").into_bytes();
  part.extend_from_slice(content);
  part.extend_from_slice(b"\r\n");
  part
}

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
use charliebot_compat::fastapi_form::{self, Field, FormModel};
use charliebot_compat::responses;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

const BOUNDARY: &str = "synthetic-form-boundary";
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

struct PythonServer {
  child: Child,
}

impl PythonServer {
  async fn start(address: SocketAddr) -> Self {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../../../..").canonicalize().expect("the worktree root exists");
    let script = manifest.join("tests/fixtures/fastapi_form_app.py");
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
    let child = command.spawn().expect("uv run starts the FastAPI form test app");
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
          panic!("FastAPI form test app exited with {status}: {stderr}");
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
    .expect("FastAPI form test app starts within 10 seconds");
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
async fn fastapi_form_matches_fastapi_for_the_multipart_corpus() {
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
  assert_eq!(cases.len(), 32);
  eprintln!("multipart differential corpus: {} cases", cases.len());
  for case in &cases {
    let (python_status, python_body) = send(&client, python_address, case).await;
    let (rust_status, rust_body) = send(&client, rust_address, case).await;

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
  let (parts, body) = request.into_parts();
  let form = match fastapi_form::read(&parts.headers, body).await {
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

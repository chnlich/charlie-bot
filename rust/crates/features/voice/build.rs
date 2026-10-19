// The test and example binaries of this crate load the sherpa-onnx runtime from their own directory. The sys crate
// copies the runtime into the profile directory and its `examples/`, but cargo builds test binaries into `deps/`, so
// this script copies the runtime there too. The build dependency on sherpa-onnx orders this script after the sys
// crate's build script, which makes the copy.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

const RUNTIME_LIBS: [&str; 2] = ["libsherpa-onnx-c-api.so", "libonnxruntime.so"];

fn main() {
  println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
  let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
  let profile = env::var("PROFILE").expect("cargo sets PROFILE");
  let profile_dir = out_dir
    .ancestors()
    .find(|path| path.file_name() == Some(OsStr::new(&profile)))
    .expect("OUT_DIR lies under the profile directory")
    .to_path_buf();
  let deps_dir = profile_dir.join("deps");
  fs::create_dir_all(&deps_dir).expect("create the deps directory");
  for lib in RUNTIME_LIBS {
    let source = profile_dir.join(lib);
    println!("cargo:rerun-if-changed={}", source.display());
    fs::copy(&source, deps_dir.join(lib)).unwrap_or_else(|err| panic!("copy {} into deps: {err}", source.display()));
  }
}

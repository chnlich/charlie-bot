// The server loads the sherpa-onnx runtime from its own directory: the sys crate copies it there. Cargo applies this
// link argument to the server's binary only, so the voice crate's build script cannot give it the RUNPATH.
fn main() {
  println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
}

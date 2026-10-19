// The server loads the sherpa-onnx runtime from its own directory: the sys crate copies it there. Cargo applies this
// link argument to the server's binary only, so the voice crate's build script cannot give it the RUNPATH.
fn main() {
  println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
  // The sys crate's link directive for libonnxruntime.so is dropped under --as-needed, because no object of the server
  // references it. Naming it again with --no-as-needed puts it in the binary's NEEDED list.
  println!("cargo:rustc-link-arg-bins=-Wl,--no-as-needed,-lonnxruntime,--as-needed");
}

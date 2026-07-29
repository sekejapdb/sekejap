fn main() {
    // Emits the platform linker args a napi addon needs (on macOS: allow the
    // Node runtime to resolve napi symbols at load time).
    napi_build::setup();
}

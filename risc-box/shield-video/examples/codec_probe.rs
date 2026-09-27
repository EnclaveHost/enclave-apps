// Shares the exact codec test with the native integration test.
#[path = "../tests/codec.rs"]
mod codec;
fn main() {
    codec::run_cases();
}

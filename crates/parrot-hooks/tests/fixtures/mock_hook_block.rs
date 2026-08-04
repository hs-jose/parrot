// Mock external hook: emits a block action.
// Drains stdin (the JSON envelope) so the pipe closes cleanly, then writes
// one JSON line to stdout representing the HookAction to apply.
use std::io::{Read, Write};

fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    let out = r#"{"action":"block","reason":"test-block"}"#;
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

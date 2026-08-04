// Mock external hook: writes a noop action but exits 1 to trigger fail-open.
use std::io::{Read, Write};

fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    let _ = std::io::stdout().write_all(b"{\"action\":\"noop\"}\n");
    let _ = std::io::stdout().flush();
    std::process::exit(1);
}

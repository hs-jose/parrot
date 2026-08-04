// Mock external hook: silent allow — empty stdout, exit 0.
use std::io::Read;

fn main() {
    let mut _buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut _buf);
    // stdout stays empty; daemon treats this as silent-allow NoOp.
}

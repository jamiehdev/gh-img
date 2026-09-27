//! `cargo run --example process -- IN OUT` re-encodes one file, for
//! scripts/check-fixtures.sh.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, output] = args.as_slice() else {
        eprintln!("usage: process IN OUT");
        std::process::exit(2);
    };

    let bytes = std::fs::read(input).expect("read input");
    match gh_img::process(&bytes) {
        Ok((out, _)) => std::fs::write(output, out).expect("write output"),
        Err(e) => {
            eprintln!("{input}: {e}");
            std::process::exit(1);
        }
    }
}

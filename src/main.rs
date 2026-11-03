// SPDX-License-Identifier: MIT

fn main() {
    match batlens_core::sys::sample() {
        Ok(s) => match s.percent {
            Some(p) => println!("{p}%"),
            None => println!("N/A"),
        },
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

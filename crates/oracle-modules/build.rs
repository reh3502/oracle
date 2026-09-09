fn main() {
    println!(
        "cargo:rustc-env=ORACLE_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}

#[path = "../../common.rs"]
mod common;

#[tokio::main(worker_threads = 2)]
async fn main() {
    common::run("fixture.alpha", env!("CARGO_PKG_VERSION"), false).await;
}

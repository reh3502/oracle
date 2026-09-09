#[path = "../../common.rs"]
mod common;

#[tokio::main(worker_threads = 2)]
async fn main() {
    common::run("fixture.beta", env!("CARGO_PKG_VERSION"), true).await;
}

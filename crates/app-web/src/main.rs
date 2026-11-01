#[tokio::main]
async fn main() {
    if let Err(e) = app_web::run().await {
        eprintln!("dam-web fatal: {e}");
        std::process::exit(1);
    }
}

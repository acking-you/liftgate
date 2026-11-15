mod app;
pub(crate) use app::*;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    app::entry().await
}

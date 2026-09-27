use log::info;
use shadowstep::config::Config;
use shadowstep::tls::bind_listeners;
use shadowstep::{build_state, run, run_metrics};
use std::io;
use std::net::TcpListener;
use std::num::NonZeroUsize;

#[actix_web::main]
async fn main() -> io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));

    let config = Config::load();
    let state = build_state(&config)?;

    let num_workers = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    info!(
        "shadowstep starting on {} with {} workers",
        config.listen_addr, num_workers
    );

    let (http, tls) = bind_listeners(&config)?;
    let proxy = run(state.clone(), http, tls, num_workers)?;

    let Some(metrics_addr) = &config.metrics_addr else {
        return proxy.await;
    };
    info!("Serving /metrics on {}", metrics_addr);
    let metrics = run_metrics(state, TcpListener::bind(metrics_addr)?)?;
    // each server stops on SIGINT or SIGTERM, so both end together
    tokio::try_join!(proxy, metrics).map(|_| ())
}

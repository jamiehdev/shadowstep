use log::info;
use shadowstep::config::Config;
use shadowstep::tls::load_rustls_config;
use shadowstep::{build_state, run};
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

    let http = TcpListener::bind(&config.listen_addr)?;

    let tls = match (config.tls_cert_path.as_ref(), config.tls_key_path.as_ref()) {
        (Some(cert_path), Some(key_path)) => {
            let tls_config = load_rustls_config(cert_path, key_path)?;
            Some((TcpListener::bind("0.0.0.0:8443")?, tls_config))
        }
        _ => None,
    };

    run(state, http, tls, num_workers)?.await
}

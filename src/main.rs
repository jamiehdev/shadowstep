use log::info;
use shadowstep::config::Config;
use shadowstep::tls::bind_listeners;
use shadowstep::{build_state, run};
use std::io;
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

    run(state, http, tls, num_workers)?.await
}

mod api_server;
pub mod db;
pub mod mdns_advertiser;
pub mod schema;

use crate::db::DatabaseHandler;
use crate::mdns_advertiser::MdnsAdvertiser;
use std::sync::{Arc, Mutex};

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let database = DatabaseHandler::start().unwrap_or_else(|e| {
        panic!("Database handler crashed! Reason: {e}");
    });

    log::info!("Database handler loaded.");
    let database = Arc::new(Mutex::new(database));

    let api_thread = api_server::spawn_api_server(database)
        .expect("Failed to spawn API server thread");

    log::info!("API server started.");

    match api_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("API server stopped with error: {e}"),
        Err(_) => eprintln!("API server thread panicked"),
    }

    match MdnsAdvertiser::start(&MdnsAdvertiser::new("Tempmon Server")) {
        Ok(handle) => {
            if let Err(e) = handle.join() {
                log::warn!("mDNS advertiser thread panicked, but ignoring: {:?}", e);
            } else {
                log::info!("mDNS advertiser thread exited gracefully.");
            }
        }
        Err(e) => {
            log::warn!("Failed to start mDNS advertiser, but continuing: {:?}", e);
        }
    }

    log::info!("MDNS advertiser started (or bypassed).");
}
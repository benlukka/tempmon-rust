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

    match MdnsAdvertiser::start(&MdnsAdvertiser::new("Tempmon Server")) {
        Ok(_mdns_handle) => {
            log::info!("MDNS advertiser running in the background.");
        }
        Err(e) => {
            log::error!("Failed to start mDNS advertiser: {:?}", e);
        }
    }
    
    log::info!("API server started.");

    match api_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("API server stopped with error: {e}"),
        Err(_) => eprintln!("API server thread panicked"),
    }


}
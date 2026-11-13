use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::error::Error;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct MdnsAdvertiser {
    name: String,
}

impl MdnsAdvertiser {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub fn start(&self) -> Result<JoinHandle<()>, Box<dyn Error>> {
        log::info!("mdns advertiser starting");
        let mdns = ServiceDaemon::new()?;

        let service_type = "_http._tcp.local.";
        let instance_name = &self.name;
        let host_name = "tempmon.local.";
        let port = 3000;

        let properties = [("version", "2.0.0")];

        let service_info = ServiceInfo::new(
            service_type,
            instance_name,
            host_name,
            "127.0.0.1",
            port,
            &properties[..],
        )?;

        mdns.register(service_info.enable_addr_auto())?;

        log::info!(
            "Successfully registered mDNS service: {} on port {}",
            instance_name,
            port
        );

        let handle = thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(10));
            }
        });

        Ok(handle)
    }
}

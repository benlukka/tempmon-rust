use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;

/// mDNS service type that ArduinoOTA registers.
pub const OTA_SERVICE_TYPE: &str = "_arduino._tcp.local.";
/// Default ArduinoOTA port.
pub const DEFAULT_DEVICE_PORT: u16 = 3232;
/// Upper bound for an accepted image (ESP32 app partitions are at most 4MB).
pub const MAX_FIRMWARE_BYTES: usize = 4 * 1024 * 1024;

const CMD_FLASH: u32 = 0;
const CMD_AUTH: u32 = 200;
const CHUNK_SIZE: usize = 1024;
/// First byte of every ESP32 application image.
const ESP_IMAGE_MAGIC: u8 = 0xE9;

#[derive(Debug, Clone)]
pub struct OtaConfig {
    pub device_password: Option<String>,
    pub api_token: Option<String>,
    pub discovery_timeout: Duration,
    pub invite_timeout: Duration,
    pub invite_tries: u32,
    pub accept_timeout: Duration,
    pub chunk_timeout: Duration,
    pub result_timeout: Duration,
    pub pio_project_dir: PathBuf,
    pub pio_env: String,
    pub wifi_ssid: String,
    pub wifi_password: String,
    server_mdns_host: String,
    server_port: u16,
    server_ip_fallback: String,
    version: String,
}

impl Default for OtaConfig {
    fn default() -> Self {
        Self {
            device_password: None,
            api_token: None,
            discovery_timeout: Duration::from_secs(5),
            invite_timeout: Duration::from_secs(3),
            invite_tries: 5,
            accept_timeout: Duration::from_secs(10),
            chunk_timeout: Duration::from_secs(10),
            result_timeout: Duration::from_secs(60),
            pio_project_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("firmware"),
            pio_env: "esp32dev".to_string(),
            wifi_ssid: std::env::var("WIFI_SSID").unwrap_or_default(),
            wifi_password: std::env::var("WIFI_PASSWORD").unwrap_or_default(),
            server_mdns_host: "tempmon".to_string(),
            server_port: 9247,
            server_ip_fallback: "127.0.0.1".to_string(),
            version: "1.0.0".to_string(),
        }
    }
}

impl OtaConfig {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            device_password: var("TEMPMON_OTA_PASSWORD"),
            api_token: var("TEMPMON_OTA_TOKEN"),
            ..Self::default()
        }
    }
}

#[derive(Debug)]
pub enum OtaError {
    Disabled,
    Unauthorized,
    InvalidFirmware(String),
    InvalidVersion(String),
    VersionUnknown { current: Option<String> },
    DeviceNotFound(String),
    NoResponse,
    PasswordRequired,
    AuthFailed,
    Rejected(String),
    Transfer(String),
    Discovery(String),
    Io(io::Error),
}

impl fmt::Display for OtaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "OTA is disabled: set TEMPMON_OTA_TOKEN on the server"),
            Self::Unauthorized => write!(f, "missing or invalid bearer token"),
            Self::InvalidFirmware(why) => write!(f, "invalid firmware image: {why}"),
            Self::InvalidVersion(raw) => write!(
                f,
                "invalid version '{raw}': expected dotted numbers such as 1.2.3"
            ),
            Self::VersionUnknown { current: None } => write!(
                f,
                "no firmware version is on record for this device; pass force=true to push anyway"
            ),
            Self::VersionUnknown { current: Some(raw) } => write!(
                f,
                "the version on record ('{raw}') is not a version number; pass force=true to push anyway"
            ),
            Self::DeviceNotFound(name) => {
                write!(
                    f,
                    "device '{name}' was not found via mDNS ({OTA_SERVICE_TYPE})"
                )
            }
            Self::NoResponse => write!(f, "the device did not answer the OTA invitation"),
            Self::PasswordRequired => {
                write!(
                    f,
                    "the device requires an OTA password: set TEMPMON_OTA_PASSWORD"
                )
            }
            Self::AuthFailed => write!(f, "the device rejected the OTA password"),
            Self::Rejected(reply) => write!(f, "the device rejected the update: {reply}"),
            Self::Transfer(why) => write!(f, "transfer failed: {why}"),
            Self::Discovery(why) => write!(f, "mDNS error: {why}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for OtaError {}

impl From<io::Error> for OtaError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Debug, Clone)]
pub struct OtaReport {
    pub target: SocketAddr,
    pub bytes: usize,
    pub md5: String,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: Vec<u64>,
    pre: Option<String>,
}

impl Version {
    pub fn parse(raw: &str) -> Result<Self, OtaError> {
        let invalid = || OtaError::InvalidVersion(raw.to_string());
        let s = raw.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let s = s.split('+').next().unwrap_or_default();
        let (numbers, pre) = match s.split_once('-') {
            Some((numbers, pre)) if !pre.is_empty() => (numbers, Some(pre.to_string())),
            Some(_) => return Err(invalid()),
            None => (s, None),
        };

        let mut core = numbers
            .split('.')
            .map(|part| part.parse::<u64>().map_err(|_| invalid()))
            .collect::<Result<Vec<_>, _>>()?;
        while core.last() == Some(&0) {
            core.pop();
        }
        Ok(Self { core, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VersionCheck<'a> {
    pub new: &'a str,
    pub current: Option<&'a str>,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Update,
    Skip(String),
}

impl VersionCheck<'_> {
    pub fn decide(&self) -> Result<Decision, OtaError> {
        if self.force {
            return Ok(Decision::Update);
        }
        let new = Version::parse(self.new)?;
        let current_raw = self
            .current
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or(OtaError::VersionUnknown { current: None })?;
        let current = Version::parse(current_raw).map_err(|_| OtaError::VersionUnknown {
            current: Some(current_raw.to_string()),
        })?;

        if new > current {
            Ok(Decision::Update)
        } else {
            Ok(Decision::Skip(format!(
                "the device already runs {current_raw}, and {} is not higher",
                self.new.trim()
            )))
        }
    }
}

#[derive(Debug, Clone)]
pub enum OtaOutcome {
    Updated(OtaReport),
    Skipped { reason: String },
}

pub struct OtaService {
    config: OtaConfig,
    busy: Mutex<()>,
}

impl OtaService {
    pub fn new(config: OtaConfig) -> Self {
        Self {
            config,
            busy: Mutex::new(()),
        }
    }

    pub fn from_env() -> Self {
        Self::new(OtaConfig::from_env())
    }

    pub fn is_enabled(&self) -> bool {
        self.config.api_token.is_some()
    }

    pub fn authorize(&self, authorization: Option<&str>) -> Result<(), OtaError> {
        let Some(expected) = self.config.api_token.as_deref() else {
            return Err(OtaError::Disabled);
        };
        let provided = authorization
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            Ok(())
        } else {
            Err(OtaError::Unauthorized)
        }
    }

    pub async fn discover(&self, device: &str) -> Result<SocketAddr, OtaError> {
        let device = device.trim_end_matches(".local").trim_end_matches('.');
        let wanted = format!("{device}.{OTA_SERVICE_TYPE}").to_lowercase();

        let daemon = ServiceDaemon::new().map_err(|e| OtaError::Discovery(e.to_string()))?;
        let receiver = daemon
            .browse(OTA_SERVICE_TYPE)
            .map_err(|e| OtaError::Discovery(e.to_string()))?;

        let deadline = tokio::time::Instant::now() + self.config.discovery_timeout;
        let found = loop {
            match tokio::time::timeout_at(deadline, receiver.recv_async()).await {
                Ok(Ok(ServiceEvent::ServiceResolved(info))) => {
                    if info.get_fullname().to_lowercase() == wanted
                        && let Some(ip) = info.get_addresses_v4().into_iter().min()
                    {
                        break Some(SocketAddr::from((ip, info.get_port())));
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break None,
            }
        };

        let _ = daemon.stop_browse(OTA_SERVICE_TYPE);
        let _ = daemon.shutdown();

        let target = found.ok_or_else(|| OtaError::DeviceNotFound(device.to_string()))?;
        log::info!("OTA: {device} resolved to {target}");
        Ok(target)
    }

    pub async fn discover_all(&self) -> Result<Vec<(String, SocketAddr)>, OtaError> {
        let daemon = ServiceDaemon::new().map_err(|e| OtaError::Discovery(e.to_string()))?;
        let receiver = daemon
            .browse(OTA_SERVICE_TYPE)
            .map_err(|e| OtaError::Discovery(e.to_string()))?;

        let mut discovered_devices = Vec::new();
        let mut seen_ips = std::collections::HashSet::new();

        let deadline = tokio::time::Instant::now() + self.config.discovery_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, receiver.recv_async()).await {
                Ok(Ok(ServiceEvent::ServiceResolved(info))) => {
                    if let Some(ip) = info.get_addresses_v4().into_iter().min() {
                        let addr = SocketAddr::from((ip, info.get_port()));
                        // Ensure unique IP addresses
                        if seen_ips.insert(ip) {
                            let name = info
                                .get_fullname()
                                .split('.')
                                .next()
                                .unwrap_or("esp32-device")
                                .to_string();
                            discovered_devices.push((name, addr));
                        }
                    }
                }
                Ok(Ok(_)) => {}
                // Timed out or the daemon went away.
                Ok(Err(_)) | Err(_) => break,
            }
        }

        let _ = daemon.stop_browse(OTA_SERVICE_TYPE);
        let _ = daemon.shutdown();

        log::info!(
            "OTA: Discovered {} unique devices on the network.",
            discovered_devices.len()
        );
        Ok(discovered_devices)
    }

    pub async fn push_to_device(
        &self,
        device: &str,
        ip: Option<Ipv4Addr>,
        firmware: &[u8],
        versions: VersionCheck<'_>,
    ) -> Result<OtaOutcome, OtaError> {
        validate_image(firmware)?;

        if let Decision::Skip(reason) = versions.decide()? {
            log::info!("OTA: not pushing to {device}: {reason}");
            return Ok(OtaOutcome::Skipped { reason });
        }

        let target = match ip {
            Some(ip) => SocketAddr::from((ip, DEFAULT_DEVICE_PORT)),
            None => self.discover(device).await?,
        };
        self.push(target, firmware).await.map(OtaOutcome::Updated)
    }

    async fn push(&self, target: SocketAddr, firmware: &[u8]) -> Result<OtaReport, OtaError> {
        validate_image(firmware)?;
        let _guard = self.busy.lock().await;

        let started = Instant::now();
        let digest = md5_hex(firmware);
        log::info!(
            "OTA: pushing {} bytes (md5 {digest}) to {target}",
            firmware.len()
        );

        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        let local_port = listener.local_addr()?.port();

        let udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        udp.connect(target).await?;

        let invitation = format!("{CMD_FLASH} {local_port} {} {digest}\n", firmware.len());
        let reply = self.invite_device(&udp, &invitation).await?;

        let auth_nonce = reply.strip_prefix("AUTH").map(|n| n.trim().to_string());
        let accepted = match auth_nonce {
            Some(nonce) => {
                self.authenticate(&udp, &nonce, firmware.len(), &digest, target.ip())
                    .await?;
                true
            }
            None => reply == "OK",
        };
        if !accepted {
            return Err(OtaError::Rejected(reply));
        }

        let mut stream = self.accept_device(&listener, target.ip()).await?;
        self.upload_to_device(&mut stream, firmware).await?;

        let report = OtaReport {
            target,
            bytes: firmware.len(),
            md5: digest,
            elapsed: started.elapsed(),
        };
        log::info!(
            "OTA: {target} accepted the update in {:.1?}",
            report.elapsed
        );
        Ok(report)
    }

    async fn invite_device(&self, udp: &UdpSocket, invitation: &str) -> Result<String, OtaError> {
        let mut buf = [0u8; 64];
        for attempt in 1..=self.config.invite_tries {
            udp.send(invitation.as_bytes()).await?;
            match timeout(self.config.invite_timeout, udp.recv(&mut buf)).await {
                Ok(Ok(n)) => return Ok(text(&buf[..n])),
                Ok(Err(e)) => {
                    log::debug!("OTA: invitation attempt {attempt} failed: {e}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(_) => log::debug!("OTA: no answer to invitation (attempt {attempt})"),
            }
        }
        Err(OtaError::NoResponse)
    }

    async fn authenticate(
        &self,
        udp: &UdpSocket,
        nonce: &str,
        size: usize,
        digest: &str,
        device_ip: IpAddr,
    ) -> Result<(), OtaError> {
        let password = self
            .config
            .device_password
            .as_deref()
            .ok_or(OtaError::PasswordRequired)?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let cnonce = md5_hex(format!("{size}{digest}{device_ip}{now}"));
        let response = md5_hex(format!("{}:{nonce}:{cnonce}", md5_hex(password)));

        udp.send(format!("{CMD_AUTH} {cnonce} {response}\n").as_bytes())
            .await?;

        let mut buf = [0u8; 64];
        match timeout(Duration::from_secs(10), udp.recv(&mut buf)).await {
            Ok(Ok(n)) if text(&buf[..n]) == "OK" => Ok(()),
            Ok(Ok(_)) => Err(OtaError::AuthFailed),
            Ok(Err(e)) => Err(OtaError::Io(e)),
            Err(_) => Err(OtaError::NoResponse),
        }
    }

    async fn accept_device(
        &self,
        listener: &TcpListener,
        expected: IpAddr,
    ) -> Result<TcpStream, OtaError> {
        let deadline = tokio::time::Instant::now() + self.config.accept_timeout;
        loop {
            match tokio::time::timeout_at(deadline, listener.accept()).await {
                Ok(Ok((stream, peer))) if peer.ip() == expected => {
                    stream.set_nodelay(true)?;
                    return Ok(stream);
                }
                Ok(Ok((_, peer))) => log::warn!("OTA: ignoring connection from {peer}"),
                Ok(Err(e)) => return Err(OtaError::Io(e)),
                Err(_) => {
                    return Err(OtaError::Transfer("the device did not connect back".into()));
                }
            }
        }
    }

    async fn upload_to_device(
        &self,
        stream: &mut TcpStream,
        firmware: &[u8],
    ) -> Result<(), OtaError> {
        let total = firmware.len();
        let mut sent = 0usize;
        let mut next_log = 25usize;
        let mut buf = [0u8; 32];
        let mut last_ack_ok = false;

        for chunk in firmware.chunks(CHUNK_SIZE) {
            timeout(self.config.chunk_timeout, stream.write_all(chunk))
                .await
                .map_err(|_| OtaError::Transfer("timed out sending firmware".into()))??;
            sent += chunk.len();

            let n = timeout(self.config.chunk_timeout, stream.read(&mut buf))
                .await
                .map_err(|_| OtaError::Transfer("timed out waiting for the device".into()))??;
            if n == 0 {
                break;
            }
            last_ack_ok = text(&buf[..n]).contains("OK");

            let percent = sent * 100 / total;
            if percent >= next_log {
                log::info!("OTA: {percent}%");
                next_log += 25;
            }
        }

        if last_ack_ok {
            return Ok(());
        }

        for _ in 0..5 {
            match timeout(self.config.result_timeout, stream.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) if text(&buf[..n]).contains("OK") => return Ok(()),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(OtaError::Io(e)),
            }
        }
        Err(OtaError::Transfer(format!(
            "the device did not confirm the update after {sent} of {total} bytes"
        )))
    }

    pub async fn build_firmware(&self, device_name: &str) -> Result<Vec<u8>, OtaError> {
        log::info!(
            "OTA: Preparing isolated build environment for device '{}'...",
            device_name
        );

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_project_dir =
            std::env::temp_dir().join(format!("tempmon_firmware_{}_{}", device_name, nanos));

        let src_dir = self.config.pio_project_dir.clone();
        let temp_dir_clone = temp_project_dir.clone();
        tokio::task::spawn_blocking(move || copy_dir_all(&src_dir, &temp_dir_clone))
            .await
            .map_err(|e| OtaError::Transfer(format!("Failed to spawn blocking copy task: {e}")))?
            .map_err(|e| {
                OtaError::Transfer(format!("Failed to copy firmware project to temp dir: {e}"))
            })?;

        let config_path = temp_project_dir.join("include/config.h");
        if let Some(parent) = config_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let content = format!(
            r#"#pragma once

#include <Arduino.h>

// ---- Auto-generated by OTA server - Do not edit manually ------------------

// ---- Wi-Fi & Identity -----------------------------------------------------
#define WIFI_SSID "{}"
#define WIFI_PASSWORD "{}"
#define DEVICE_HOSTNAME "{}"
#define DEVICE_ID       DEVICE_HOSTNAME    // sent as "device" with every measurement
#define FIRMWARE_VERSION "{}"           // sent as the "Version" header

// ---- DHT22 ----------------------------------------------------------------
#define DHT_PIN 4

// ---- Timing ---------------------------------------------------------------
#define READ_INTERVAL_MS         300000UL   // time between uploads
#define RETRY_INTERVAL_MS         3000UL   // retry after a failed sensor read (DHT22 needs >= 2 s)
#define WIFI_CONNECT_TIMEOUT_MS  20000UL   // how long setup() waits for Wi-Fi
#define WIFI_RESTART_AFTER_MS    60000UL   // force a fresh WiFi.begin() after this long offline

// ---- tempmon server -------------------------------------------------------
#define SERVER_MDNS_HOST        "{}"
#define SERVER_PORT             {}
#define SERVER_PATH             "/api/measurements"
#define SERVER_FALLBACK_IP      "{}"
#define MDNS_QUERY_TIMEOUT_MS   2000
#define HTTP_TIMEOUT_MS         5000
"#,
            self.config.wifi_ssid,
            self.config.wifi_password,
            device_name,
            self.config.version,
            self.config.server_mdns_host,
            self.config.server_port,
            self.config.server_ip_fallback
        );

        tokio::fs::write(&config_path, content).await?;
        log::info!("OTA: Generated isolated config header at {:?}", config_path);

        // 4. Run PlatformIO build inside the temporary directory
        log::info!(
            "OTA: Running PlatformIO build for environment '{}' in temp dir...",
            self.config.pio_env
        );

        let mut child = Command::new("pio")
            .arg("run")
            .arg("-e")
            .arg(&self.config.pio_env)
            .current_dir(&temp_project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                dbg!(OtaError::Transfer(format!(
                    "Failed to execute PlatformIO CLI: {e}"
                )))
            })?;

        let stdout = child.stdout.take().expect("Child process must have stdout");
        let stderr = child.stderr.take().expect("Child process must have stderr");

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let stdout_task = tokio::spawn(async move {
            while let Ok(Some(line)) = stdout_reader.next_line().await {
                log::info!("[PIO] {}", line);
            }
        });

        let stderr_task = tokio::spawn(async move {
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                log::error!("[PIO] {}", line);
            }
        });

        let status = child.wait().await.map_err(|e| {
            dbg!(OtaError::Transfer(format!(
                "Failed to wait on PlatformIO process: {e}"
            )))
        })?;

        let _ = stdout_task.await;
        let _ = stderr_task.await;

        if !status.success() {
            let _ = tokio::fs::remove_dir_all(&temp_project_dir).await;
            return Err(dbg!(OtaError::Transfer(format!(
                "PlatformIO compilation failed with status {}",
                status
            ))));
        }

        // 5. Read the compiled firmware binary from the temporary build directory
        let firmware_path = temp_project_dir
            .join(".pio")
            .join("build")
            .join(&self.config.pio_env)
            .join("firmware.bin");

        let firmware_bytes = tokio::fs::read(&firmware_path).await.map_err(|e| {
            dbg!(OtaError::Transfer(format!(
                "Failed to read compiled firmware at {:?}: {e}",
                firmware_path
            )))
        })?;

        // 6. Clean up the temporary project directory
        let _ = tokio::fs::remove_dir_all(&temp_project_dir).await;

        Ok(firmware_bytes)
    }

    pub async fn build_and_push_to_device(
        &self,
        device_slug: &str,
        versions: VersionCheck<'_>,
    ) -> Result<OtaOutcome, OtaError> {
        let device_name = device_slug.trim_end_matches(".local").trim_end_matches('.');

        if let Decision::Skip(reason) = versions.decide()? {
            log::info!("OTA: not pushing to {device_name}: {reason}");
            return Ok(OtaOutcome::Skipped { reason });
        }

        let target = self.discover(device_name).await?;
        let firmware = self.build_firmware(device_name).await?;
        self.push(target, &firmware).await.map(OtaOutcome::Updated)
    }
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            if entry.file_name() == ".pio" || entry.file_name() == ".git" {
                continue;
            }
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

fn validate_image(firmware: &[u8]) -> Result<(), OtaError> {
    if firmware.is_empty() {
        return Err(OtaError::InvalidFirmware("empty body".into()));
    }
    if firmware.len() > MAX_FIRMWARE_BYTES {
        return Err(OtaError::InvalidFirmware(format!(
            "{} bytes exceeds the limit",
            firmware.len()
        )));
    }
    if firmware[0] != ESP_IMAGE_MAGIC {
        return Err(OtaError::InvalidFirmware(
            "not an ESP32 app image (expected firmware.bin, first byte 0xE9)".into(),
        ));
    }
    Ok(())
}

fn md5_hex(data: impl AsRef<[u8]>) -> String {
    format!("{:x}", md5::compute(data))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::task::JoinHandle;

    fn test_config(password: Option<&str>) -> OtaConfig {
        OtaConfig {
            device_password: password.map(String::from),
            api_token: Some("secret-token".into()),
            discovery_timeout: Duration::from_secs(1),
            invite_timeout: Duration::from_millis(300),
            invite_tries: 2,
            accept_timeout: Duration::from_secs(2),
            chunk_timeout: Duration::from_secs(2),
            result_timeout: Duration::from_secs(2),
            pio_project_dir: PathBuf::from("./firmware_project"),
            pio_env: "esp32dev".to_string(),
            wifi_ssid: "".to_string(),
            wifi_password: "".to_string(),
            server_mdns_host: "".to_string(),
            server_port: 0,
            server_ip_fallback: "".to_string(),
            version: "".to_string(),
        }
    }

    fn image() -> Vec<u8> {
        let mut fw = vec![ESP_IMAGE_MAGIC];
        fw.extend((0..5000u32).map(|i| (i % 251) as u8));
        fw
    }

    async fn fake_esp(
        device_password: Option<&'static str>,
    ) -> (SocketAddr, JoinHandle<Option<Vec<u8>>>) {
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = udp.local_addr().unwrap();

        let task = tokio::spawn(async move {
            let mut buf = [0u8; 256];
            let (n, peer) = udp.recv_from(&mut buf).await.unwrap();
            let invite = String::from_utf8_lossy(&buf[..n]).to_string();
            let mut parts = invite.split_whitespace();
            assert_eq!(parts.next(), Some("0"));
            let port: u16 = parts.next().unwrap().parse().unwrap();
            let size: usize = parts.next().unwrap().parse().unwrap();
            let digest = parts.next().unwrap().to_string();

            if let Some(password) = device_password {
                let nonce = md5_hex("nonce-from-device");
                udp.send_to(format!("AUTH {nonce}").as_bytes(), peer)
                    .await
                    .unwrap();

                let (n, _) = udp.recv_from(&mut buf).await.unwrap();
                let msg = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut p = msg.split_whitespace();
                assert_eq!(p.next(), Some("200"));
                let cnonce = p.next().unwrap();
                let response = p.next().unwrap();
                let expected = md5_hex(format!("{}:{nonce}:{cnonce}", md5_hex(password)));
                if response != expected {
                    udp.send_to(b"Authentication Failed", peer).await.unwrap();
                    return None;
                }
            }
            udp.send_to(b"OK", peer).await.unwrap();

            let mut tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let mut image = Vec::new();
            let mut chunk = [0u8; 2048];
            while image.len() < size {
                let n = tcp.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                image.extend_from_slice(&chunk[..n]);
                tcp.write_all(image.len().to_string().as_bytes())
                    .await
                    .unwrap();
            }
            assert_eq!(md5_hex(&image), digest);
            tcp.write_all(b"OK").await.unwrap();
            Some(image)
        });
        (addr, task)
    }

    #[tokio::test]
    async fn pushes_without_password() {
        let (addr, esp) = fake_esp(None).await;
        let fw = image();
        let report = OtaService::new(test_config(None))
            .push(addr, &fw)
            .await
            .unwrap();
        assert_eq!(report.bytes, fw.len());
        assert_eq!(esp.await.unwrap(), Some(fw));
    }

    #[tokio::test]
    async fn pushes_with_password() {
        let (addr, esp) = fake_esp(Some("hunter2")).await;
        let fw = image();
        OtaService::new(test_config(Some("hunter2")))
            .push(addr, &fw)
            .await
            .unwrap();
        assert_eq!(esp.await.unwrap(), Some(fw));
    }

    #[tokio::test]
    async fn wrong_password_is_reported() {
        let (addr, esp) = fake_esp(Some("hunter2")).await;
        let err = OtaService::new(test_config(Some("nope")))
            .push(addr, &image())
            .await
            .unwrap_err();
        assert!(matches!(err, OtaError::AuthFailed), "{err}");
        assert_eq!(esp.await.unwrap(), None);
    }

    #[tokio::test]
    async fn missing_password_is_reported() {
        let (addr, esp) = fake_esp(Some("hunter2")).await;
        let err = OtaService::new(test_config(None))
            .push(addr, &image())
            .await
            .unwrap_err();
        assert!(matches!(err, OtaError::PasswordRequired), "{err}");
        esp.abort();
    }

    #[tokio::test]
    async fn silent_device_gives_no_response() {
        let addr = UdpSocket::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let err = OtaService::new(test_config(None))
            .push(addr, &image())
            .await
            .unwrap_err();
        assert!(matches!(err, OtaError::NoResponse), "{err}");
    }

    #[tokio::test]
    async fn rejects_things_that_are_not_esp_images() {
        let svc = OtaService::new(test_config(None));
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        for bad in [
            vec![],
            vec![0x00, 1, 2, 3],
            vec![ESP_IMAGE_MAGIC; MAX_FIRMWARE_BYTES + 1],
        ] {
            let err = svc.push(addr, &bad).await.unwrap_err();
            assert!(matches!(err, OtaError::InvalidFirmware(_)), "{err}");
        }
    }

    fn v(raw: &str) -> Version {
        Version::parse(raw).unwrap()
    }

    fn check<'a>(new: &'a str, current: Option<&'a str>, force: bool) -> VersionCheck<'a> {
        VersionCheck {
            new,
            current,
            force,
        }
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(v("1.10.0") > v("1.9.0"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.0.1") > v("1.0"));
        assert!(v("1.0.0.1") > v("1.0.0"));
        assert!(v("1.0.0") < v("1.0.1"));
    }

    #[test]
    fn versions_normalise() {
        assert_eq!(v("1.0"), v("1.0.0"));
        assert_eq!(v("1"), v("1.0.0.0"));
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v(" V1.2.3 "), v("1.2.3"));
        assert_eq!(v("1.2.3+build.7"), v("1.2.3"));
        assert_eq!(v("0"), v("0.0.0"));
    }

    #[test]
    fn prereleases_rank_below_the_release() {
        assert!(v("1.0.0-rc1") < v("1.0.0"));
        assert!(v("1.0.0-rc1") > v("0.9.9"));
        assert!(v("1.0.0-rc2") > v("1.0.0-rc1"));
        assert!(v("1.0.1-rc1") > v("1.0.0"));
    }

    #[test]
    fn bad_versions_are_rejected() {
        for bad in [
            "",
            " ",
            "abc",
            "1.x",
            "1..2",
            "1.2.",
            "-1.0",
            "1.0.0-",
            "v",
            "1.2.3.beta",
        ] {
            assert!(
                matches!(Version::parse(bad), Err(OtaError::InvalidVersion(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn only_a_higher_version_updates() {
        assert_eq!(
            check("1.0.1", Some("1.0.0"), false).decide().unwrap(),
            Decision::Update
        );
        assert_eq!(
            check("1.10.0", Some("1.9.0"), false).decide().unwrap(),
            Decision::Update
        );
        assert_eq!(
            check("1.0.0", Some("1.0.0-rc1"), false).decide().unwrap(),
            Decision::Update
        );

        for (new, current) in [
            ("1.0.0", "1.0.0"),
            ("1.0", "1.0.0"),
            ("1.0.0", "1.0.1"),
            ("1.9.0", "1.10.0"),
        ] {
            let decision = check(new, Some(current), false).decide().unwrap();
            assert!(
                matches!(decision, Decision::Skip(_)),
                "{new} over {current}: {decision:?}"
            );
        }
    }

    #[test]
    fn unknown_current_version_is_refused_unless_forced() {
        for current in [None, Some(""), Some("  "), Some("unknown")] {
            let err = check("2.0.0", current, false).decide().unwrap_err();
            assert!(
                matches!(err, OtaError::VersionUnknown { .. }),
                "{current:?}: {err}"
            );
            assert_eq!(
                check("2.0.0", current, true).decide().unwrap(),
                Decision::Update
            );
        }
    }

    #[test]
    fn invalid_new_version_is_refused_unless_forced() {
        let err = check("latest", Some("1.0.0"), false).decide().unwrap_err();
        assert!(matches!(err, OtaError::InvalidVersion(_)), "{err}");
    }

    #[test]
    fn force_allows_equal_and_lower() {
        assert_eq!(
            check("1.0.0", Some("1.0.0"), true).decide().unwrap(),
            Decision::Update
        );
        assert_eq!(
            check("0.5.0", Some("1.0.0"), true).decide().unwrap(),
            Decision::Update
        );
    }

    #[tokio::test]
    async fn skipped_update_never_contacts_the_device() {
        let svc = OtaService::new(test_config(None));
        let ip = Some(Ipv4Addr::LOCALHOST);
        let outcome = svc
            .push_to_device("dev", ip, &image(), check("1.0.0", Some("1.0.0"), false))
            .await
            .unwrap();
        assert!(matches!(outcome, OtaOutcome::Skipped { .. }), "{outcome:?}");

        let err = svc
            .push_to_device("dev", ip, &image(), check("1.0.1", None, false))
            .await
            .unwrap_err();
        assert!(
            matches!(err, OtaError::VersionUnknown { current: None }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn higher_version_goes_on_to_contact_the_device() {
        let svc = OtaService::new(test_config(None));
        let err = svc
            .push_to_device(
                "dev",
                Some(Ipv4Addr::LOCALHOST),
                &image(),
                check("1.0.1", Some("1.0.0"), false),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, OtaError::NoResponse), "{err}");
    }

    #[test]
    fn token_check() {
        let svc = OtaService::new(test_config(None));
        assert!(svc.authorize(Some("Bearer secret-token")).is_ok());
        assert!(matches!(
            svc.authorize(Some("Bearer wrong")),
            Err(OtaError::Unauthorized)
        ));
        assert!(matches!(
            svc.authorize(Some("secret-token")),
            Err(OtaError::Unauthorized)
        ));
        assert!(matches!(svc.authorize(None), Err(OtaError::Unauthorized)));

        let disabled = OtaService::new(OtaConfig::default());
        assert!(!disabled.is_enabled());
        assert!(matches!(
            disabled.authorize(Some("Bearer x")),
            Err(OtaError::Disabled)
        ));
    }
}

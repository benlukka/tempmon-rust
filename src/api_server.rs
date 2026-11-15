use crate::db::{Database, Device, Measurement, MeasurementType, run_db};
use poem_openapi::__private::poem;
use poem_openapi::__private::poem::Route;
use poem_openapi::__private::poem::http::{HeaderMap, StatusCode};
use poem_openapi::__private::poem::listener::TcpListener;
use poem_openapi::param::Query;
use poem_openapi::payload::{Json, PlainText};
use poem_openapi::{Object, OpenApi, OpenApiService};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::{fs, io};

pub struct Api {
    pub database: Arc<Mutex<Database>>,
}
#[derive(Object)]
pub struct MeasurementInput {
    pub device: String,
    pub capability: MeasurementType,
    pub value: serde_json::Value,
}

#[OpenApi]
impl Api {
    #[oai(path = "/db-version", method = "get")]
    async fn db_version(&self) -> poem::Result<PlainText<String>> {
        let version = run_db(&self.database, |db| db.version()).await?;
        Ok(PlainText(version.to_string()))
    }

    #[oai(path = "/measurements", method = "post")]
    async fn handle_measurement(
        &self,
        headers: &HeaderMap,
        body: Json<Vec<MeasurementInput>>,
    ) -> poem::Result<()> {
        let hostname = headers
            .get("Hostname")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                dbg!(poem::Error::from_string(
                    "No Hostname was provided in the Header",
                    StatusCode::BAD_REQUEST
                ))
            })?;
        let capabilities =
            if let Some(capability_str) = headers.get("Capability").and_then(|v| v.to_str().ok()) {
                let parsed: Result<Vec<MeasurementType>, String> = capability_str
                    .split(',')
                    .map(|s| s.parse::<MeasurementType>())
                    .collect();

                match parsed {
                    Ok(caps) if !caps.is_empty() => caps,
                    _ => {
                        return Err(dbg!(poem::Error::from_string(
                            "Invalid Capability provided in Header",
                            StatusCode::BAD_REQUEST,
                        )));
                    }
                }
            } else {
                return Err(dbg!(poem::Error::from_string(
                    "No Capability was provided in the Header",
                    StatusCode::BAD_REQUEST,
                )));
            };

        // 3. Extract and validate Version
        let version = headers
            .get("Version")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                dbg!(poem::Error::from_string(
                    "No Version was provided in the Header",
                    StatusCode::BAD_REQUEST
                ))
            })?;

        let rows: Vec<(String, MeasurementType, serde_json::Value)> = body
            .0
            .into_iter()
            .map(|m| (m.device, m.capability, m.value))
            .collect();

        run_db(&self.database, move |db| {
            db.save_measurement_for_device(hostname, version, capabilities, &rows)
        })
        .await?;
        Ok(())
    }

    #[oai(path = "/measurements", method = "get")]
    async fn get_latest_measurements(
        &self,
        limit: Query<i64>,
    ) -> poem::Result<Json<Vec<Measurement>>> {
        let measurements = run_db(&self.database, move |db| {
            db.retrieve_latest_measurements(limit.0)
        })
        .await?;
        Ok(Json(measurements))
    }

    #[oai(path = "/measurements", method = "get")]
    async fn retrieve_measurements_between(
        &self,
        start_date: Query<chrono::DateTime<chrono::Local>>,
        end_date: Query<chrono::DateTime<chrono::Local>>,
    ) -> poem::Result<Json<Vec<Measurement>>> {
        let measurements = run_db(&self.database, move |db| {
            db.retrieve_measurements_between(start_date.0, end_date.0)
        })
        .await?;
        Ok(Json(measurements))
    }

    #[oai(path = "/devices", method = "get")]
    async fn get_devices(&self) -> poem::Result<Json<Vec<Device>>> {
        let devices = run_db(&self.database, |db| db.get_devices()).await?;
        Ok(Json(devices))
    }
}

pub fn spawn_api_server(database: Arc<Mutex<Database>>) -> io::Result<JoinHandle<io::Result<()>>> {
    log::info!("Starting API server.");

    let handle = std::thread::Builder::new()
        .name("api-server".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            runtime.block_on(async {
                let api_service = OpenApiService::new(Api { database }, "Tempmon Server", "2.0")
                    .server("http://localhost:9247/api");

                let ui = api_service.swagger_ui();
                let spec_json = api_service.spec();
                fs::write("api.json", &spec_json)?;

                let app = Route::new().nest("/api", api_service).nest("/", ui);

                poem::Server::new(TcpListener::bind("0.0.0.0:9247"))
                    .run(app)
                    .await
            })
        })?;

    Ok(handle)
}

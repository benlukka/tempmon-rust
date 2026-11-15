use crate::schema::*;
use chrono::Local;
use diesel::connection::DatabaseVersion;
use diesel::prelude::*;
use diesel_derive_enum::DbEnum;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use poem::Error;
use poem::error::InternalServerError;
use poem::http::StatusCode;
use poem_openapi::{Enum, Object};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[derive(DbEnum, Debug, Clone, PartialEq, Enum)]
#[db_enum(
    existing_type_path = "crate::schema::sql_types::MeasurementType",
    value_style = "snake_case"
)]
pub enum MeasurementType {
    Temperature,
    Humidity,
    Voltage,
    Pressure,
}
impl FromStr for MeasurementType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "temperature" => Ok(MeasurementType::Temperature),
            "humidity" => Ok(MeasurementType::Humidity),
            "voltage" => Ok(MeasurementType::Voltage),
            "pressure" => Ok(MeasurementType::Pressure),
            other => Err(format!("Unknown capability: {}", other)),
        }
    }
}

#[derive(Queryable, Debug, Clone, Selectable, Object)]
#[diesel(table_name = crate::schema::devices)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Device {
    pub id: i32,
    pub unique_identifier: String,
    pub name: String,
    pub version: Option<String>,
}

#[derive(Insertable)]
#[diesel(table_name = crate::schema::devices)]
pub struct NewDevice<'a> {
    pub unique_identifier: &'a str,
    pub name: &'a str,
    pub version: Option<&'a str>,
}

#[derive(Queryable, Debug, Clone, Selectable, Object)]
#[diesel(table_name = crate::schema::measurements)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Measurement {
    pub id: i32,
    pub device: String,
    pub capabilities: MeasurementType,
    pub value: serde_json::Value,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Insertable)]
#[diesel(table_name = crate::schema::measurements)]
pub struct NewMeasurement<'a> {
    pub device: &'a str,
    pub capabilities: MeasurementType,
    pub value: serde_json::Value,
}

pub struct DatabaseHandler {}

impl DatabaseHandler {
    pub fn start() -> Result<Database, Box<dyn std::error::Error + Send + Sync>> {
        let connection =
            PgConnection::establish("postgresql://postgres:postgres@localhost/postgres")?;
        let mut database = Database { connection };
        database
            .run_migrations()
            .expect("Database migrations failed");
        Ok(database)
    }
}
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

pub struct Database {
    connection: PgConnection,
}
impl Database {
    fn run_migrations(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        log::info!("Starting database migrations");
        self.connection.run_pending_migrations(MIGRATIONS)?;
        let diesel_migration = self.connection.applied_migrations();
        log::info!(
            "Total database migrations: {}. Last Migration: {:?}",
            diesel_migration.iter().count(),
            diesel_migration?
                .first()
                .expect("Database migrations not found")
        );
        Ok(())
    }

    pub fn version(&self) -> QueryResult<DatabaseVersion> {
        self.connection.version()
    }

    pub fn get_devices(&mut self) -> QueryResult<Vec<Device>> {
        devices::table
            .order(devices::id.desc())
            .select(Device::as_select())
            .load(&mut self.connection)
    }

    pub fn retrieve_latest_measurements(&mut self, limit: i64) -> QueryResult<Vec<Measurement>> {
        measurements::table
            .order(measurements::recorded_at.desc())
            .limit(limit)
            .select(Measurement::as_select())
            .load(&mut self.connection)
    }

    pub fn retrieve_measurements_between(
        &mut self,
        star_date: chrono::DateTime<Local>,
        end_date: chrono::DateTime<Local>,
    ) -> QueryResult<Vec<Measurement>> {
        measurements::table
            .filter(measurements::recorded_at.ge(star_date))
            .filter(measurements::recorded_at.le(end_date))
            .order(measurements::recorded_at.desc())
            .select(Measurement::as_select())
            .load(&mut self.connection)
    }

    pub fn save_measurement_for_device(
        &mut self,
        device_hostname: String,
        device_version: String,
        device_capabilities: Vec<MeasurementType>,
        measurements_data: &[(String, MeasurementType, serde_json::Value)],
    ) -> Result<(), diesel::result::Error> {
        log::info!(
            "received {} measurements for device: {}",
            measurements_data.len(),
            device_hostname
        );
        log::info!(
            "Device version: {}, capabilities: {:?}",
            device_version,
            device_capabilities
        );

        if measurements_data.is_empty() {
            log::warn!("Empty measurement list handed to save");
            return Ok(());
        }

        let distinct_devices: std::collections::BTreeSet<&str> = measurements_data
            .iter()
            .map(|(d, _, _)| d.as_str())
            .collect();

        let new_devices: Vec<NewDevice> = distinct_devices
            .into_iter()
            .map(|d| NewDevice {
                unique_identifier: d,
                name: d,
                version: Some(device_version.as_str()),
            })
            .collect();

        let new_measurements: Vec<NewMeasurement> = measurements_data
            .iter()
            .map(|(device_id, capability, val)| NewMeasurement {
                device: device_id.as_str(),
                capabilities: capability.clone(),
                value: val.clone(),
            })
            .collect();

        self.connection.transaction(|conn| {
            diesel::insert_into(devices::table)
                .values(&new_devices)
                .on_conflict(devices::unique_identifier)
                .do_update()
                .set(devices::version.eq(device_version.clone()))
                .execute(conn)?;

            diesel::insert_into(measurements::table)
                .values(&new_measurements)
                .execute(conn)?;

            log::info!("Successfully saved measurements and device records.");
            Ok(())
        })
    }
}

pub async fn run_db<F, T, E>(db: &Arc<Mutex<Database>>, f: F) -> poem::Result<T>
where
    F: FnOnce(&mut Database) -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let db_clone = Arc::clone(db);

    // Run inside spawn_blocking, returning Result<T, String>
    let join_result = tokio::task::spawn_blocking(move || {
        let mut guard = db_clone
            .lock()
            .map_err(|_| "Database lock poisoned".to_string())?;

        f(&mut guard).map_err(|e| e.to_string())
    })
    .await;

    //Handle the Tokio JoinError (if the thread panicked)
    let db_result = join_result.map_err(InternalServerError)?;

    //
    let data = db_result.map_err(|e| {
        log::error!("{:}", e);
        dbg!(Error::from_string(e, StatusCode::INTERNAL_SERVER_ERROR))
    })?;
    Ok(data)
}

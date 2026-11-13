pub mod sql_types {
    #[derive(diesel::sql_types::SqlType)]
    #[diesel(postgres_type(name = "measurement_type"))]
    pub struct MeasurementType;
}

diesel::table! {
    devices (id) {
        id -> Int4,
        unique_identifier -> Varchar,
        name -> Varchar,
        version -> Nullable<Varchar>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::MeasurementType;

    measurements (id) {
        id -> Int4,
        device -> Varchar,
        capabilities -> MeasurementType,
        value -> Jsonb,
        recorded_at -> Timestamptz,
    }
}

diesel::allow_tables_to_appear_in_same_query!(devices, measurements,);

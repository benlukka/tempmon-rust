DO $$
BEGIN
CREATE TYPE measurement_type AS ENUM ('temperature', 'humidity', 'voltage', 'pressure');
EXCEPTION
    WHEN duplicate_object THEN NULL;
END $$;

CREATE TABLE IF NOT EXISTS devices (
                                       id                SERIAL PRIMARY KEY,
                                       unique_identifier VARCHAR(255) UNIQUE NOT NULL,
    name              VARCHAR(255) NOT NULL,
    version           VARCHAR(100)
    );

CREATE TABLE IF NOT EXISTS measurements (
                                            id          SERIAL PRIMARY KEY,
                                            device      VARCHAR(255) NOT NULL
    REFERENCES devices (unique_identifier) ON DELETE CASCADE,
    capabilities measurement_type NOT NULL,
    value       JSONB NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
    );

CREATE INDEX IF NOT EXISTS idx_measurements_device_recorded_at
    ON measurements (device, recorded_at DESC);
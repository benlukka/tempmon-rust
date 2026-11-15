// ESP32 + DHT22 client for tempmon-rust.
//
// - reads a DHT22 and POSTs temperature + humidity to the tempmon server
// - finds the server via mDNS (tempmon.local); if that fails, or the resolved
//   address can't be reached, it falls back to SERVER_FALLBACK_IP
// - ArduinoOTA for wireless firmware updates

#include <Arduino.h>
#include <ArduinoOTA.h>
#include <DHT.h>
#include <ESPmDNS.h>
#include <HTTPClient.h>
#include <WiFi.h>

#include "config.h"

static DHT dht(DHT_PIN, DHT22);

static uint32_t nextReadAt = 0;
static uint32_t offlineSince = 0;
static bool otaStarted = false;

static bool due(uint32_t deadline) {
  return (int32_t)(millis() - deadline) >= 0;
}

// ---------------------------------------------------------------------------
// Wi-Fi
// ---------------------------------------------------------------------------

static void startWifi() {
  WiFiClass::setHostname(DEVICE_HOSTNAME);
  WiFiClass::mode(WIFI_STA);
  WiFi.setAutoReconnect(true);
  WiFi.begin(WIFI_SSID, WIFI_PASSWORD);
}

static void maintainWifi() {
  if (WiFiClass::status() == WL_CONNECTED) {
    offlineSince = 0;
    return;
  }
  if (offlineSince == 0) {
    offlineSince = millis();
    if (offlineSince == 0) offlineSince = 1;
  }
  // Auto-reconnect normally recovers on its own; this is the safety net.
  if (millis() - offlineSince > WIFI_RESTART_AFTER_MS) {
    Serial.println("Wi-Fi offline for too long, restarting connection");
    WiFi.disconnect();
    WiFi.begin(WIFI_SSID, WIFI_PASSWORD);
    offlineSince = millis();
  }
}

// ---------------------------------------------------------------------------
// OTA
// ---------------------------------------------------------------------------

static void startOta() {
  ArduinoOTA.setHostname(DEVICE_HOSTNAME);

  ArduinoOTA.onStart([]() { Serial.println("OTA: start"); });
  ArduinoOTA.onEnd([]() { Serial.println("\nOTA: done, rebooting"); });
  ArduinoOTA.onProgress([](unsigned int done, unsigned int total) {
    Serial.printf("OTA: %u%%\r", done * 100 / total);
  });
  ArduinoOTA.onError([](ota_error_t error) {
    Serial.printf("OTA error [%u]\n", (unsigned)error);
  });

  // begin() also starts the mDNS responder (tempmon-esp32.local), which is
  // what MDNS.queryHost() below relies on, so it has to run before any lookup.
  ArduinoOTA.begin();
  otaStarted = true;
  Serial.printf("OTA ready: %s.local\n", DEVICE_HOSTNAME);
}

// ---------------------------------------------------------------------------
// Server lookup + upload
// ---------------------------------------------------------------------------

static bool isUsable(const IPAddress &ip) {
  return ip != IPAddress(static_cast<uint32_t>(0)) && ip != IPAddress((uint32_t)0xFFFFFFFF);
}

static IPAddress resolveServer() {
  IPAddress ip = MDNS.queryHost(SERVER_MDNS_HOST, MDNS_QUERY_TIMEOUT_MS);
  if (isUsable(ip)) {
    Serial.printf("mDNS: %s.local -> %s\n", SERVER_MDNS_HOST, ip.toString().c_str());
  } else {
    Serial.printf("mDNS: could not resolve %s.local\n", SERVER_MDNS_HOST);
  }
  return ip;
}

enum class PostResult { Ok, Rejected, Unreachable };

static PostResult postTo(const IPAddress &server, const String &capabilities, const String &body) {
  HTTPClient http;
  http.setConnectTimeout(HTTP_TIMEOUT_MS);
  http.setTimeout(HTTP_TIMEOUT_MS);

  if (!http.begin(server.toString(), SERVER_PORT, SERVER_PATH)) {
    return PostResult::Unreachable;
  }
  http.addHeader("Content-Type", "application/json");
  http.addHeader("Hostname", DEVICE_HOSTNAME);   // required by the server
  http.addHeader("Capability", capabilities);    // required, comma separated
  http.addHeader("Version", FIRMWARE_VERSION);   // required by the server

  int code = http.POST(body);
  http.end();

  if (code < 0) {
    Serial.printf("POST to %s failed: %s\n", server.toString().c_str(),
                  HTTPClient::errorToString(code).c_str());
    return PostResult::Unreachable;
  }
  if (code >= 200 && code < 300) {
    Serial.printf("POST to %s -> %d\n", server.toString().c_str(), code);
    return PostResult::Ok;
  }
  Serial.printf("POST to %s rejected with HTTP %d\n", server.toString().c_str(), code);
  return PostResult::Rejected;
}

static bool upload(float temperature, float humidity) {
  char json[192];
  snprintf(json, sizeof(json),
           "[{\"device\":\"%s\",\"capability\":\"Temperature\",\"value\":%.1f},"
           "{\"device\":\"%s\",\"capability\":\"Humidity\",\"value\":%.1f}]",
           DEVICE_ID, temperature, DEVICE_ID, humidity);
  const String body(json);
  const String capabilities = "Temperature,Humidity";

  // 1) mDNS first.
  IPAddress resolved = resolveServer();
  if (isUsable(resolved)) {
    PostResult r = postTo(resolved, capabilities, body);
    // A reachable server that answers with an error won't be any different
    // on the fallback address, so only "couldn't connect" moves on.
    if (r != PostResult::Unreachable) return r == PostResult::Ok;
    Serial.println("Resolved address unreachable, trying fallback IP");
  } else {
    Serial.println("Using fallback IP");
  }

  // 2) Hardcoded fallback.
  IPAddress fallback;
  if (!fallback.fromString(SERVER_FALLBACK_IP) || !isUsable(fallback)) {
    Serial.println("SERVER_FALLBACK_IP is not a valid address");
    return false;
  }
  if (fallback == resolved) return false;  // already tried this one
  return postTo(fallback, capabilities, body) == PostResult::Ok;
}

// ---------------------------------------------------------------------------
// Arduino entry points
// ---------------------------------------------------------------------------

void setup() {
  Serial.begin(115200);
  delay(200);
  Serial.printf("\ntempmon-esp32 %s\n", FIRMWARE_VERSION);

  dht.begin();

  startWifi();
  Serial.printf("Connecting to Wi-Fi '%s'", WIFI_SSID);
  uint32_t start = millis();
  while (WiFiClass::status() != WL_CONNECTED && millis() - start < WIFI_CONNECT_TIMEOUT_MS) {
    delay(250);
    Serial.print('.');
  }
  if (WiFiClass::status() == WL_CONNECTED) {
    Serial.printf("\nConnected, IP %s\n", WiFi.localIP().toString().c_str());
  } else {
    Serial.println("\nNot connected yet, will keep trying in the background");
  }

  nextReadAt = millis() + 2500;  // DHT22 needs a moment after power-up
}

void loop() {
  maintainWifi();

  if (WiFiClass::status() == WL_CONNECTED) {
    if (!otaStarted) startOta();
    ArduinoOTA.handle();

    if (due(nextReadAt)) {
      float t = dht.readTemperature();
      float h = dht.readHumidity();

      if (isnan(t) || isnan(h)) {
        Serial.println("DHT22 read failed, retrying shortly");
        nextReadAt = millis() + RETRY_INTERVAL_MS;
      } else {
        Serial.printf("%.1f C, %.1f %%\n", t, h);
        if (!upload(t, h)) Serial.println("Upload failed, will try again next interval");
        nextReadAt = millis() + READ_INTERVAL_MS;
      }
    }
  }

  delay(10);
}

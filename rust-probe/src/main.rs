// ============================================================================
//  Phase 0 体积探针 —— 不做业务逻辑，只为量出「WiFi + HTTPS + MQTT/TLS」
//  在 size profile 下的真实固件体积。
//
//  凭据一律用占位串：体积在链接期就确定了，不需要真连上。
// ============================================================================

use std::time::Duration;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::mqtt::client::{EspMqttClient, MqttClientConfiguration};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sntp::EspSntp;
use esp_idf_svc::sys::esp_crt_bundle_attach;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};

use log::info;

const SSID: &str = "PLACEHOLDER";
const PASS: &str = "PLACEHOLDER";
const MQTT_URI: &str = "mqtts://example.invalid:8883";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ---- SNTP（体积相关：拉进 lwip sntp + 定时器路径）----
    let sntp = EspSntp::new_default()?;
    info!("SNTP 启动");
    let _ = sntp;

    // ---- Wi-Fi STA ----
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: SSID.try_into().unwrap(),
        password: PASS.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;

    wifi.start()?;
    info!("Wi-Fi 已启动");

    // 探针不阻塞等连上：连不上也要产出体积数字
    if wifi.connect().is_ok() {
        let _ = wifi.wait_netif_up();
        info!("Wi-Fi 已连接");
    } else {
        info!("Wi-Fi 未连上（探针不关心）");
    }

    // ---- HTTPS（体积大头：mbedtls + 证书包 + http client）----
    if let Ok(mut conn) = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(esp_crt_bundle_attach),
        timeout: Some(Duration::from_secs(10)),
        ..Default::default()
    }) {
        let _ = conn.initiate_request(esp_idf_svc::http::Method::Get, "/", &[]);
        let _ = conn.initiate_response();
        info!("HTTPS 路径已链接");
    }

    // ---- MQTT over TLS（体积大头：mqtt 组件 + tls transport）----
    let _mqtt = EspMqttClient::new_cb(
        MQTT_URI,
        &MqttClientConfiguration {
            crt_bundle_attach: Some(esp_crt_bundle_attach),
            keep_alive_interval: Some(Duration::from_secs(60)),
            ..Default::default()
        },
        |event| {
            let _ = event;
        },
    );

    info!("探针启动完成 —— 现在只需量体积，不需要真连上");
    std::thread::sleep(Duration::from_secs(5));
    Ok(())
}

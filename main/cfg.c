#include "cfg.h"
#include "app_config.h"
#include "secrets.h"

#include <string.h>
#include <stdio.h>
#include <stdlib.h>

#include "nvs.h"
#include "esp_log.h"

static const char *TAG = "cfg";

// NVS 命名空间。改名字会导致老设备读不到已有配置，别随便动。
#define CFG_NVS_NS  "psw_cfg"

// ---- 内存里的当前值 ----
static char    s_ssid[33];       // 802.11 SSID 最长 32 字节 + NUL
static char    s_pass[65];       // WPA2 密码最长 63 + NUL
static int8_t  s_tx_dbm;
static uint16_t s_pulse_ms;
static uint16_t s_cmd_ttl;
static uint16_t s_status_s;
static uint16_t s_keepalive_s;

// ---------------------------------------------------------------------------
//  读取辅助：读不到（首次上电 / 键不存在）就返回默认值。
//  这是"烧录一次就够"的关键 —— NVS 空的也能正常工作。
// ---------------------------------------------------------------------------
static void load_str(nvs_handle_t h, const char *key, char *out,
                     size_t out_sz, const char *dflt)
{
    size_t len = out_sz;
    esp_err_t err = nvs_get_str(h, key, out, &len);
    if (err != ESP_OK) {
        strlcpy(out, dflt, out_sz);
    }
}

static uint16_t load_u16(nvs_handle_t h, const char *key, uint16_t dflt)
{
    uint16_t v = dflt;
    nvs_get_u16(h, key, &v);    // 读不到就保持 dflt
    return v;
}

static int8_t load_i8(nvs_handle_t h, const char *key, int8_t dflt)
{
    int8_t v = dflt;
    nvs_get_i8(h, key, &v);
    return v;
}

// 把内存里的值重新用编译期默认值填满，然后再从 NVS 覆盖。
static void load_all(void)
{
    nvs_handle_t h;
    esp_err_t err = nvs_open(CFG_NVS_NS, NVS_READONLY, &h);
    if (err != ESP_OK) {
        // 命名空间还不存在（首次上电）：全用默认值。
        ESP_LOGI(TAG, "NVS 无 '%s' 命名空间，使用编译期默认值", CFG_NVS_NS);
        strlcpy(s_ssid, WIFI_SSID, sizeof(s_ssid));
        strlcpy(s_pass, WIFI_PASS, sizeof(s_pass));
        s_tx_dbm      = WIFI_TX_POWER_DBM;
        s_pulse_ms    = PULSE_MS_DEFAULT;
        s_cmd_ttl     = CMD_TTL_DEFAULT;
        s_status_s    = STATUS_PUBLISH_S;
        s_keepalive_s = MQTT_KEEPALIVE_S;
        return;
    }

    load_str(h, "ssid", s_ssid, sizeof(s_ssid), WIFI_SSID);
    load_str(h, "pass", s_pass, sizeof(s_pass), WIFI_PASS);
    s_tx_dbm      = load_i8 (h, "tx_dbm",    WIFI_TX_POWER_DBM);
    s_pulse_ms    = load_u16(h, "pulse_ms",  PULSE_MS_DEFAULT);
    s_cmd_ttl     = load_u16(h, "cmd_ttl",   CMD_TTL_DEFAULT);
    s_status_s    = load_u16(h, "status_s",  STATUS_PUBLISH_S);
    s_keepalive_s = load_u16(h, "keepalive", MQTT_KEEPALIVE_S);

    nvs_close(h);
}

void cfg_init(void)
{
    load_all();
    ESP_LOGI(TAG, "配置已载入: ssid='%s' pass=%s tx=%d dBm pulse=%u ms ttl=%u s status=%u s",
             s_ssid, s_pass[0] ? "***" : "(空)",
             (int)s_tx_dbm, (unsigned)s_pulse_ms,
             (unsigned)s_cmd_ttl, (unsigned)s_status_s);
}

// ---------------------------------------------------------------------------
//  写入辅助
// ---------------------------------------------------------------------------
static esp_err_t save_u16(const char *key, uint16_t v)
{
    nvs_handle_t h;
    esp_err_t err = nvs_open(CFG_NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_u16(h, key, v);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

// ---------------------------------------------------------------------------
//  Wi-Fi
// ---------------------------------------------------------------------------
const char *cfg_wifi_ssid(void) { return s_ssid; }
const char *cfg_wifi_pass(void) { return s_pass; }

esp_err_t cfg_set_wifi(const char *ssid, const char *pass)
{
    if (!ssid || !pass) return ESP_ERR_INVALID_ARG;
    if (strlen(ssid) > 32) return ESP_ERR_INVALID_ARG;
    if (strlen(pass) > 63) return ESP_ERR_INVALID_ARG;

    nvs_handle_t h;
    esp_err_t err = nvs_open(CFG_NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;

    err = nvs_set_str(h, "ssid", ssid);
    if (err == ESP_OK) err = nvs_set_str(h, "pass", pass);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);

    if (err == ESP_OK) {
        strlcpy(s_ssid, ssid, sizeof(s_ssid));
        strlcpy(s_pass, pass, sizeof(s_pass));
        ESP_LOGI(TAG, "Wi-Fi 已保存: ssid='%s' pass=%s",
                 s_ssid, s_pass[0] ? "***" : "(空)");
    }
    return err;
}

// ---------------------------------------------------------------------------
//  发射功率
// ---------------------------------------------------------------------------
int8_t cfg_tx_power_dbm(void) { return s_tx_dbm; }

esp_err_t cfg_set_tx_power_dbm(int8_t dbm)
{
    // 驱动范围 [8,84] * 0.25dBm = 2..20 dBm。超出会被驱动拒绝，
    // 这里先挡一道，给出人话错误。
    if (dbm < 2 || dbm > 20) return ESP_ERR_INVALID_ARG;
    esp_err_t err = save_u16("tx_dbm", (uint16_t)dbm);
    if (err == ESP_OK) s_tx_dbm = dbm;
    return err;
}

// ---------------------------------------------------------------------------
//  脉冲宽度
// ---------------------------------------------------------------------------
uint16_t cfg_pulse_ms(void) { return s_pulse_ms; }

esp_err_t cfg_set_pulse_ms(uint16_t ms)
{
    // 安全边界仍然是编译期常量：运行时改不出这个范围。
    if (ms < PULSE_MS_MIN || ms > PULSE_MS_MAX) return ESP_ERR_INVALID_ARG;
    esp_err_t err = save_u16("pulse_ms", ms);
    if (err == ESP_OK) s_pulse_ms = ms;
    return err;
}

// ---------------------------------------------------------------------------
//  指令时间窗
// ---------------------------------------------------------------------------
uint16_t cfg_cmd_ttl(void) { return s_cmd_ttl; }

esp_err_t cfg_set_cmd_ttl(uint16_t s)
{
    if (s < 5 || s > 300) return ESP_ERR_INVALID_ARG;
    esp_err_t err = save_u16("cmd_ttl", s);
    if (err == ESP_OK) s_cmd_ttl = s;
    return err;
}

// ---------------------------------------------------------------------------
//  状态上报周期
// ---------------------------------------------------------------------------
uint16_t cfg_status_period_s(void) { return s_status_s; }

esp_err_t cfg_set_status_period_s(uint16_t s)
{
    if (s < 5 || s > 3600) return ESP_ERR_INVALID_ARG;
    esp_err_t err = save_u16("status_s", s);
    if (err == ESP_OK) s_status_s = s;
    return err;
}

// ---------------------------------------------------------------------------
//  MQTT keepalive
// ---------------------------------------------------------------------------
uint16_t cfg_keepalive_s(void) { return s_keepalive_s; }

esp_err_t cfg_set_keepalive_s(uint16_t s)
{
    if (s < 10 || s > 3600) return ESP_ERR_INVALID_ARG;
    esp_err_t err = save_u16("keepalive", s);
    if (err == ESP_OK) s_keepalive_s = s;
    return err;
}

// ---------------------------------------------------------------------------
//  打印 / 复位
// ---------------------------------------------------------------------------
void cfg_dump(void)
{
    printf("当前配置（存于 NVS 命名空间 '%s'）:\n", CFG_NVS_NS);
    printf("  wifi.ssid      = %s\n", s_ssid);
    printf("  wifi.pass      = %s\n", s_pass[0] ? "***（已设置）" : "（空 = 开放网络）");
    printf("  wifi.tx_dbm    = %d dBm\n", (int)s_tx_dbm);
    printf("  pulse.ms       = %u ms (允许 %d..%d)\n",
           (unsigned)s_pulse_ms, PULSE_MS_MIN, PULSE_MS_MAX);
    printf("  cmd.ttl        = %u s\n", (unsigned)s_cmd_ttl);
    printf("  status.period  = %u s\n", (unsigned)s_status_s);
    printf("  mqtt.keepalive = %u s\n", (unsigned)s_keepalive_s);
}

esp_err_t cfg_reset(void)
{
    nvs_handle_t h;
    esp_err_t err = nvs_open(CFG_NVS_NS, NVS_READWRITE, &h);
    if (err == ESP_OK) {
        err = nvs_erase_all(h);
        if (err == ESP_OK) err = nvs_commit(h);
        nvs_close(h);
    }
    if (err == ESP_OK || err == ESP_ERR_NVS_NOT_FOUND) {
        load_all();
        ESP_LOGW(TAG, "已恢复编译期默认配置");
        return ESP_OK;
    }
    return err;
}

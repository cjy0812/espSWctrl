#pragma once

// ============================================================================
//  运行时可改配置（存 NVS，掉电不丢）
//
//  为什么要这一层：
//    改 Wi-Fi 密码 / 脉冲宽度这类参数，如果写死在 app_config.h，每改一次
//    就要重新编译 + 烧录。板子没有自动复位电路，烧录还得手动拔插 USB，
//    极其难受。改成存 NVS 之后，串口敲一行命令就能改，改完立刻生效。
//
//  设计边界（重要）：
//    - 只有"可调参数"进 NVS。安全边界（PULSE_MS_MIN/MAX）仍然是编译期
//      常量 —— 运行时只能在小范围内调，不能把边界撑开。
//    - 首次上电 NVS 是空的，此时用 secrets.h / app_config.h 里的编译期
//      值作为默认值。所以烧录一次之后，以后都不用再烧了。
// ============================================================================

#include <stdint.h>
#include <stdbool.h>
#include "esp_err.h"

// 初始化：打开 NVS 命名空间并载入所有配置（没有的项用编译期默认值）。
// 必须在 nvs_flash_init() 之后调用。
void cfg_init(void);

// ---- Wi-Fi ----
const char *cfg_wifi_ssid(void);
const char *cfg_wifi_pass(void);
// 保存到 NVS（不负责重连，调用方决定何时应用）
esp_err_t cfg_set_wifi(const char *ssid, const char *pass);

// ---- 发射功率（dBm，驱动单位是 0.25dBm，转换在内部做）----
int8_t cfg_tx_power_dbm(void);
esp_err_t cfg_set_tx_power_dbm(int8_t dbm);

// ---- 脉冲默认宽度（毫秒）。实际执行时仍会被 PULSE_MS_MIN/MAX 夹住 ----
uint16_t cfg_pulse_ms(void);
esp_err_t cfg_set_pulse_ms(uint16_t ms);

// ---- 指令时间窗（秒）----
uint16_t cfg_cmd_ttl(void);
esp_err_t cfg_set_cmd_ttl(uint16_t s);

// ---- 状态上报周期（秒）----
uint16_t cfg_status_period_s(void);
esp_err_t cfg_set_status_period_s(uint16_t s);

// ---- MQTT keepalive（秒，下次连接生效）----
uint16_t cfg_keepalive_s(void);
esp_err_t cfg_set_keepalive_s(uint16_t s);

// 把当前全部配置打到控制台（密码做掩码处理）
void cfg_dump(void);

// 恢复出厂默认：清空命名空间并重新载入编译期默认值
esp_err_t cfg_reset(void);

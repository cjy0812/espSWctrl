#pragma once

// ============================================================================
//  Wi-Fi 运行时控制接口
//
//  这些函数由 main.c 实现、由串口控制台调用。抽出来的目的是让
//  console_cmds.c 不必知道 esp_wifi 的内部细节（阈值、重连时序等）。
// ============================================================================

#include <stdint.h>

// 立即按当前 cfg 里的 SSID/密码重配 STA 并重连
void wifi_reconnect_with_cfg(void);

// 立即把发射功率设成 dbm（内部换算成驱动的 0.25dBm 单位）
void wifi_set_tx_power_now(int8_t dbm);

// 扫描一次并把结果打到日志
void wifi_scan_now(void);

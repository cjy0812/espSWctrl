#pragma once

#include <stdbool.h>

// ============================================================================
//  PC 开关机状态回读（第二个 817D，读 SATA 5V）
//
//  电路（架构文档 5.9）：
//    SATA 5V → 1k → 光耦1脚；2脚 → SATA GND
//    光耦4脚(C) → GPIO6（内部/外部 10k 上拉到 3.3V）
//    光耦3脚(E) → ESP32 GND
//
//  逻辑：
//    PC 关机 → SATA 无 5V → LED 灭 → 三极管截止 → GPIO6 = 高 → false
//    PC 开机 → SATA 有 5V → LED 亮 → 三极管导通 → GPIO6 = 低 → true
// ============================================================================

// 初始化 GPIO6 为输入（带上拉）
void pc_state_init(void);

// 读一次当前状态。true = PC 开着，false = PC 关着。
bool pc_state_is_on(void);
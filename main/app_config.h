#pragma once

// ============================================================================
//  powerswitch 固件配置（MVP）
//  改这里就能调，不用翻 main.c
// ============================================================================

// ---- 引脚（务必避开 strapping pin: GPIO2/8/9）----
#define PIN_PWR_SW      5   // 输出：驱动第一个 817D，模拟按下电源键
#define PIN_PC_STATE    6   // 输入：第二个 817D，读 SATA 5V 判断 PC 开关机

// ---- 脉冲参数（毫秒）----
// 主板识别阈值：>=100ms 有效；>=4000ms 触发强制断电
//
// 注意 PULSE_MS_MIN/MAX 是【安全边界】，只在这里定义，不进 NVS：
// 运行时的 pulse 命令只能在这个区间内调，改不出边界。
#define PULSE_MS_MIN        100
#define PULSE_MS_MAX        3000
#define PULSE_GAP_MIN_MS    2000    // 两次脉冲最小间隔

// 下面这几个是【首次上电的默认值】。第一次启动时 NVS 是空的，用它们；
// 之后串口改过的值存在 NVS 里，会覆盖这里。所以改这些只影响新板子 /
// reset 之后的行为，日常调整请用串口命令（见 console_cmds.c）。
#define PULSE_MS_DEFAULT    300
#define CMD_TTL_DEFAULT     30      // 秒：ts 允许的时间窗口
#define NONCE_HISTORY       16      // 记住最近多少个 nonce 用于去重

// ---- Wi-Fi 射频 ----
// 发射功率，单位 dBm。驱动内部单位是 0.25dBm，换算在 cfg.c / main.c 里做。
// C3 硬件支持 2..20 dBm；驱动会把实际值量化到 {8,20,28,...,80}（即 2/5/7/.../20 dBm）。
// 17 dBm 会被量化到 16 dBm（驱动档位 66 = 16.5dBm → 实际 16）。
#define WIFI_TX_POWER_DBM   17

// ---- MQTT ----
#define MQTT_KEEPALIVE_S    60
#define STATUS_PUBLISH_S    30      // 状态回读上报周期
#define RECONNECT_BASE_MS   1000    // 指数退避起点
#define RECONNECT_MAX_MS    60000   // 退避上限（防烧 Serverless 额度）

// ---- 功能开关 ----
#define ENABLE_STATUS_READBACK  1

// ---- 时间同步 ----
//  指令校验依赖时间窗（ts + ttl），所以 SNTP 必须成功，否则所有指令被拒。
//  不用 pool.ntp.org：国内经常能解析但 NTP 包不通。用国内可达的服务器。
#define SNTP_SERVER_1           "ntp.aliyun.com"
#define SNTP_SERVER_2           "cn.pool.ntp.org"
#define SNTP_SYNC_TIMEOUT_S     15
#define SNTP_RETRY_FAST_S       2    // 前几次重试间隔
#define SNTP_RETRY_SLOW_S       30   // 一直失败后的重试间隔   // 装了第二个光耦就设 1

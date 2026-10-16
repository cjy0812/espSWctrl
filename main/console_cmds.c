// ============================================================================
//  串口控制台命令
//
//  目的：把「改个 Wi-Fi 密码要重新编译 + 烧录」这件事彻底消掉。
//
//  用法（COM11 上敲）：
//    help                     列出所有命令
//    cfg                      打印当前配置
//    wifi <ssid> <pass>       改 Wi-Fi 并存 NVS，立即重连
//    txpower <dBm>            改发射功率（2..20），立即生效
//    pulse <ms>               改默认脉冲宽度（100..3000）
//    ttl <秒>                 改指令时间窗
//    status <秒>              改状态上报周期
//    keepalive <秒>           改 MQTT keepalive（重连后生效）
//    save                     看到「已保存」即写入成功（各命令已自带保存）
//    reset                    清空 NVS，恢复编译期默认值
//    reboot                   软重启
//
//  安全说明：
//    - 控制台命令绝不做 GPIO 操作，只走 pulse_request() 那条已有队列，
//      所以不会绕开「脉冲独立任务」这条安全线。
//    - 密码不会回显明文。
// ============================================================================

#include "cfg.h"
#include "wifi_ctrl.h"
#include "pulse.h"
#include "app_config.h"

#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <inttypes.h>

#include "esp_console.h"
#include "esp_system.h"
#include "esp_log.h"

static const char *TAG = "console";

// ---------------------------------------------------------------------------
//  help 之外的命令实现
//  统一用 argv 手工解析：命令都很短，引 argtable 反而更绕。
// ---------------------------------------------------------------------------

static int cmd_cfg(int argc, char **argv)
{
    (void)argc; (void)argv;
    cfg_dump();
    return 0;
}

static int cmd_wifi(int argc, char **argv)
{
    // 允许 `wifi <ssid>` 表示开放网络（空密码），
    // 也允许 `wifi <ssid> <pass>`。
    if (argc < 2) {
        printf("用法: wifi <ssid> [password]\n");
        printf("      不写 password = 开放网络\n");
        printf("      SSID 里有空格时用引号: wifi \"my net\" secret\n");
        return 1;
    }
    const char *ssid = argv[1];
    const char *pass = (argc >= 3) ? argv[2] : "";

    esp_err_t err = cfg_set_wifi(ssid, pass);
    if (err != ESP_OK) {
        printf("保存失败: %s\n", esp_err_to_name(err));
        return 1;
    }
    printf("已保存: ssid='%s' pass=%s\n", ssid, pass[0] ? "***" : "(空)");
    printf("正在重连…（用 cfg 命令确认，或看日志）\n");

    // 真正去连：把新配置推给 Wi-Fi 驱动并重连。
    wifi_reconnect_with_cfg();
    return 0;
}

static int cmd_txpower(int argc, char **argv)
{
    if (argc < 2) {
        printf("用法: txpower <dBm>   当前 %d dBm（范围 2..20）\n",
               (int)cfg_tx_power_dbm());
        return 1;
    }
    int dbm = atoi(argv[1]);
    if (dbm < 2 || dbm > 20) {
        printf("超出范围：驱动只支持 2..20 dBm\n");
        return 1;
    }
    esp_err_t err = cfg_set_tx_power_dbm((int8_t)dbm);
    if (err != ESP_OK) {
        printf("保存失败: %s\n", esp_err_to_name(err));
        return 1;
    }
    wifi_set_tx_power_now((int8_t)dbm);
    printf("发射功率已设为 %d dBm\n", dbm);
    return 0;
}

static int cmd_pulse(int argc, char **argv)
{
    if (argc < 2) {
        printf("用法: pulse <ms>   当前 %u ms（范围 %d..%d）\n",
               (unsigned)cfg_pulse_ms(), PULSE_MS_MIN, PULSE_MS_MAX);
        return 1;
    }
    int ms = atoi(argv[1]);
    esp_err_t err = cfg_set_pulse_ms((uint16_t)ms);
    if (err != ESP_OK) {
        printf("超出范围或保存失败：允许 %d..%d ms\n",
               PULSE_MS_MIN, PULSE_MS_MAX);
        return 1;
    }
    printf("默认脉冲宽度已设为 %d ms\n", ms);
    return 0;
}

static int cmd_ttl(int argc, char **argv)
{
    if (argc < 2) {
        printf("用法: ttl <秒>   当前 %u s（范围 5..300）\n",
               (unsigned)cfg_cmd_ttl());
        return 1;
    }
    int s = atoi(argv[1]);
    esp_err_t err = cfg_set_cmd_ttl((uint16_t)s);
    if (err != ESP_OK) {
        printf("超出范围（5..300）或保存失败\n");
        return 1;
    }
    printf("指令时间窗已设为 %d 秒\n", s);
    return 0;
}

static int cmd_status(int argc, char **argv)
{
    if (argc < 2) {
        printf("用法: status <秒>   当前 %u s（范围 5..3600）\n",
               (unsigned)cfg_status_period_s());
        return 1;
    }
    int s = atoi(argv[1]);
    esp_err_t err = cfg_set_status_period_s((uint16_t)s);
    if (err != ESP_OK) {
        printf("超出范围（5..3600）或保存失败\n");
        return 1;
    }
    printf("状态上报周期已设为 %d 秒（下一次循环生效）\n", s);
    return 0;
}

static int cmd_keepalive(int argc, char **argv)
{
    if (argc < 2) {
        printf("用法: keepalive <秒>   当前 %u s（范围 10..3600）\n",
               (unsigned)cfg_keepalive_s());
        return 1;
    }
    int s = atoi(argv[1]);
    esp_err_t err = cfg_set_keepalive_s((uint16_t)s);
    if (err != ESP_OK) {
        printf("超出范围（10..3600）或保存失败\n");
        return 1;
    }
    printf("MQTT keepalive 已设为 %d 秒（重连后生效）\n", s);
    return 0;
}

static int cmd_reset(int argc, char **argv)
{
    (void)argc; (void)argv;
    esp_err_t err = cfg_reset();
    if (err != ESP_OK) {
        printf("复位失败: %s\n", esp_err_to_name(err));
        return 1;
    }
    printf("已清空 NVS，恢复编译期默认值。\n");
    printf("Wi-Fi 需要重连才生效，敲 wifi 命令或 reboot。\n");
    cfg_dump();
    return 0;
}

static int cmd_scan(int argc, char **argv)
{
    (void)argc; (void)argv;
    printf("扫描中（约 2 秒）…\n");
    wifi_scan_now();
    return 0;
}

static int cmd_reboot(int argc, char **argv)
{
    (void)argc; (void)argv;
    printf("重启中…\n");
    // 给 stdout 一点时间把上面那行送出去，否则串口刚看到就断了。
    fflush(stdout);
    esp_restart();
    return 0;   // 不会走到
}

static int cmd_press(int argc, char **argv)
{
    // 手工触发一次电源键脉冲，用于不接 MQTT 也能验证硬件。
    uint16_t width = cfg_pulse_ms();
    if (argc >= 2) {
        int ms = atoi(argv[1]);
        if (ms < PULSE_MS_MIN || ms > PULSE_MS_MAX) {
            printf("宽度超出安全范围 %d..%d ms\n", PULSE_MS_MIN, PULSE_MS_MAX);
            return 1;
        }
        width = (uint16_t)ms;
    }
    // 走和 MQTT 完全相同的那条队列，不绕过脉冲任务。
    if (pulse_request(width) != ESP_OK) {
        printf("忙：上一次脉冲还没结束\n");
        return 1;
    }
    printf("已投递脉冲 %u ms\n", (unsigned)width);
    return 0;
}

// ---------------------------------------------------------------------------
//  注册表
// ---------------------------------------------------------------------------
static void register_all(void)
{
    const esp_console_cmd_t cmds[] = {
        { .command = "cfg",       .help = "打印当前配置",                     .func = cmd_cfg },
        { .command = "wifi",      .help = "改 Wi-Fi: wifi <ssid> [pass]",     .func = cmd_wifi },
        { .command = "txpower",   .help = "改发射功率: txpower <dBm>",        .func = cmd_txpower },
        { .command = "pulse",     .help = "改脉冲宽度: pulse <ms>",           .func = cmd_pulse },
        { .command = "ttl",       .help = "改指令时间窗: ttl <秒>",           .func = cmd_ttl },
        { .command = "status",    .help = "改状态上报周期: status <秒>",      .func = cmd_status },
        { .command = "keepalive", .help = "改 MQTT keepalive: keepalive <秒>", .func = cmd_keepalive },
        { .command = "scan",      .help = "立刻扫描一次 Wi-Fi",               .func = cmd_scan },
        { .command = "press",     .help = "手动触发一次电源键脉冲",           .func = cmd_press },
        { .command = "reset",     .help = "清空 NVS 恢复默认配置",            .func = cmd_reset },
        { .command = "reboot",    .help = "软重启",                           .func = cmd_reboot },
    };

    for (size_t i = 0; i < sizeof(cmds) / sizeof(cmds[0]); i++) {
        ESP_ERROR_CHECK(esp_console_cmd_register(&cmds[i]));
    }
    ESP_ERROR_CHECK(esp_console_register_help_command());
}

// ---------------------------------------------------------------------------
//  启动控制台
//
//  ⚠️ 这里必须用 USB Serial/JTAG 后端，原因：
//    这块板子的 COM11 是芯片自带的 USB Serial/JTAG（VID 303A:PID 1001），
//    不是 USB-UART 桥。而 USB-JTAG 只有在被设为「主控制台」时才有输入；
//    作为「副控制台」时按 IDF 文档只支持单向输出、不支持 REPL。
//    所以 sdkconfig.defaults 里必须：
//        CONFIG_ESP_CONSOLE_USB_SERIAL_JTAG=y     (primary)
//        CONFIG_ESP_CONSOLE_UART_DEFAULT 关掉
//        CONFIG_ESP_CONSOLE_SECONDARY_NONE=y
// ---------------------------------------------------------------------------
void console_start(void)
{
    esp_console_repl_t *repl = NULL;

    esp_console_repl_config_t repl_cfg = ESP_CONSOLE_REPL_CONFIG_DEFAULT();
    repl_cfg.prompt = "psw>";
    repl_cfg.max_history_len = 16;

    esp_console_dev_usb_serial_jtag_config_t dev_cfg =
        ESP_CONSOLE_DEV_USB_SERIAL_JTAG_CONFIG_DEFAULT();

    esp_err_t err = esp_console_new_repl_usb_serial_jtag(&dev_cfg, &repl_cfg, &repl);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "控制台初始化失败: %s（串口配置命令将不可用，其余功能不受影响）",
                 esp_err_to_name(err));
        return;
    }

    register_all();

    err = esp_console_start_repl(repl);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "控制台启动失败: %s", esp_err_to_name(err));
        return;
    }

    ESP_LOGI(TAG, "串口控制台已就绪 —— 敲 help 看命令");
}
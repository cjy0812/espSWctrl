// ============================================================================
//  powerswitch —— 远程按下电脑电源键
//
//  ESP32-C3 PRO Mini + 817D 光耦（PC817 系列）
//
//  安全设计的三条主线（改代码前先读这三条）：
//   1. 上电默认安全：app_main 第一件事就是把 GPIO5 置低，
//      不等 Wi-Fi、不等 MQTT。复位期间 GPIO 高阻 → 光耦断开 → PC 不受影响。
//   2. 脉冲独立任务：绝不在 MQTT 回调里拉 GPIO。回调被阻塞 → 脉冲超 4 秒
//      → 主板强制断电 → 丢未保存的工作。用队列交给 pulse_task。
//   3. 纵深防御：不假设 broker 配对了 ACL，固件自己逐字节校验 topic。
// ============================================================================

#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <inttypes.h>

#include "app_config.h"
#include "secrets.h"
#include "pulse.h"
#include "pc_state.h"
#include "cmd_verify.h"
#include "cfg.h"
#include "wifi_ctrl.h"

// 由 console_cmds.c 实现：启动串口 REPL（改 Wi-Fi / 参数不用重新烧录）
void console_start(void);

#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/event_groups.h"

#include "esp_system.h"
#include "esp_mac.h"
#include "esp_log.h"
#include "esp_wifi.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "esp_netif_sntp.h"
#include "esp_task_wdt.h"
#include "esp_crt_bundle.h"
#include "nvs_flash.h"
#include "mqtt_client.h"

static const char *TAG = "psw";

// ---------------------------------------------------------------------------
//  全局状态
// ---------------------------------------------------------------------------
static EventGroupHandle_t s_net_eg;
#define BIT_WIFI_UP   BIT0
#define BIT_TIME_SYNC BIT1

static esp_mqtt_client_handle_t s_mqtt;
static char s_device_id[32];
static char s_topic_cmd[64];
static char s_topic_state[64];
static char s_topic_status[64];
static char s_topic_ack[64];
static volatile bool s_mqtt_up;
static volatile bool s_scan_mode;   // 扫描期间抑制自动重连（驱动不允许边连边扫）

// 连续重连失败计数。官方 Station Scenarios 点名：断线后无条件立刻
// esp_wifi_connect() 会让扫描「饿死」（scan starve to death）—— 连接优先级
// 高于扫描，scan 永远抢不到空口。解法就是连续重连若干次后拉长间隔。
//
// 这个计数同时是退避的依据：前几次立即重连（AP 只是重启时能秒回），
// 连续失败越多次间隔越长（密码错 / 没有这个 AP 时不会把空口占死）。
static int s_reconnect_tries;

// 连续重连失败到第几次开始拉长间隔。前 RECONNECT_IMMEDIATE 次用
// RECONNECT_FAST_MS 快速恢复（正常路由器重启一下就能秒回），此后固定
// RECONNECT_SLOW_MS，避免连不上时还把空口占死。
// 目的是官方说的 "avoid the scan starve to death"。
#define RECONNECT_IMMEDIATE  5
#define RECONNECT_FAST_MS    500
#define RECONNECT_SLOW_MS    5000

// ============================================================================
//  Wi-Fi
//
//  SSID / 密码 / 发射功率全部来自 cfg（NVS），不再写死在 main.c 里。
//  串口控制台可以随时改，改完不用重新烧录 —— 见 console_cmds.c。
// ============================================================================

// 按当前配置填一个 STA 配置结构。
// authmode 由密码是否为空推导：空 = 开放网络，非空 = 至少 WPA2。
static void wifi_fill_config(wifi_config_t *wc)
{
    memset(wc, 0, sizeof(*wc));
    strlcpy((char *)wc->sta.ssid, cfg_wifi_ssid(), sizeof(wc->sta.ssid));
    strlcpy((char *)wc->sta.password, cfg_wifi_pass(), sizeof(wc->sta.password));

    bool secured = cfg_wifi_pass()[0] != '\0';

    // 路由器是 WPA2/WPA3 过渡模式（transition mode）。threshold 取
    // WPA2_WPA3_PSK 而不是 WPA2_PSK：头文件 wifi_scan_threshold_t.authmode
    // 规定混合模式阈值取两者中**更强**的那个（"WPA2_WPA3 becomes WPA3"），
    // 只有写 WPA2_WPA3_PSK 才会真正接受并优先选 WPA3。
    // 写 WPA2_PSK 其实也连得上这个 AP（WPA2 已达标），只是拿不到 WPA3。
    wc->sta.threshold.authmode = secured ? WIFI_AUTH_WPA2_WPA3_PSK : WIFI_AUTH_OPEN;

    // 过渡模式 AP 的信标带 PMF（802.11w）能力位，WPA3 强制要求 PMF。
    // STA 必须声明「支持」但「不强制」：
    //   capable=false → 驱动直接丢弃该 AP，表现为 reason=210（加密方式不兼容）
    //   required=true → 反过来连不上纯 WPA2 的 AP
    // 所以 capable=true / required=false 是过渡模式唯一正确的组合。
    if (secured) {
        wc->sta.pmf_cfg.capable  = true;
        wc->sta.pmf_cfg.required = false;
    }
}

static void wifi_apply_tx_power(void)
{
    int8_t dbm = cfg_tx_power_dbm();
    // 驱动的单位是 0.25 dBm，所以乘 4。
    esp_err_t err = esp_wifi_set_max_tx_power((int8_t)(dbm * 4));
    if (err != ESP_OK) {
        ESP_LOGW(TAG, "设置发射功率失败: %s", esp_err_to_name(err));
        return;
    }
    int8_t actual = 0;
    esp_wifi_get_max_tx_power(&actual);
    ESP_LOGI(TAG, "发射功率 = %d dBm（驱动实际 %d.%02d dBm）",
             (int)dbm, actual / 4, (actual % 4) * 25);
}

static void wifi_event_handler(void *arg, esp_event_base_t base,
                               int32_t id, void *data)
{
    if (base == WIFI_EVENT && id == WIFI_EVENT_STA_START) {
        // 首次连接必须在这里发起：esp_wifi_start() 只打开驱动，不会自己连。
        esp_wifi_connect();
    } else if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
        wifi_event_sta_disconnected_t *d = (wifi_event_sta_disconnected_t *)data;
        xEventGroupClearBits(s_net_eg, BIT_WIFI_UP);

        // 把真实原因码翻译成人话。这是排查 Wi-Fi 连不上唯一的可靠证据。
        const char *why = "未知";
        switch (d->reason) {
        // 2 发生在认证帧阶段（4-way 之前）：STA 发出认证请求后没等到 AP 的
        // 认证响应。不是密码错 —— 密码错会落在 15/204（握手阶段）。
        case 2:   why = "认证超时：AP 没回认证帧（加密/PMF 不匹配、AP 拒绝、或信号差）"; break;
        // 15 和 204 是同一件事：204 是 Espressif 私有的 HANDSHAKE_TIMEOUT，
        // 官方说明其成因与 15（四次握手超时）相同。最常见是密码错，
        // 但信号太差也会触发 —— 别一口咬定密码。
        case 15:  why = "四次握手超时（最常见是密码错，也可能是信号太差）"; break;
        case 200: why = "信标丢失（信号太弱）"; break;
        case 201: why = "没找到这个 AP（SSID 错 / 5GHz？）"; break;
        case 202: why = "认证被 AP 拒绝（MAC 过滤 / 加密方式不兼容）"; break;
        case 203: why = "关联被 AP 拒绝（PMF 强制 / 连接数满）"; break;
        case 204: why = "四次握手超时（同 15：密码错或信号太差）"; break;
        case 205: why = "连接失败"; break;
        case 210: why = "找到 AP，但加密方式不兼容（很可能是 WPA3）"; break;
        case 211: why = "找到 AP，但加密方式低于我们设的阈值"; break;
        case 212: why = "找到 AP，但信号低于阈值"; break;
        default:  break;
        }
        ESP_LOGW(TAG, "Wi-Fi 断开: reason=%d (0x%x) —— %s",
                 d->reason, d->reason, why);

        // 扫描期间不要抢着重连：驱动不允许「连接中」扫描，
        // 自动重连一开，scan 命令就会永远返回 ESP_ERR_WIFI_STATE。
        if (s_scan_mode) {
            ESP_LOGI(TAG, "扫描模式：暂不自动重连");
            return;
        }

        // 重连退避。官方 Station Scenarios 点名：断线后无条件立刻
        // esp_wifi_connect() 会让扫描饿死 —— 连接优先级高于扫描，scan
        // 永远抢不到空口。官方给的解法就是「前 N 次快速重连，之后拉长间隔」，
        // 目的是 "avoid the scan starve to death"。
        //
        // 前 5 次给 500ms（快速恢复，正常路由器重启一下就回来了），
        // 第 6 次起给 5 秒（此时基本可以确定是连不上，别再空转抢空口）。
        //
        // SSID / 密码一律来自 cfg（NVS），不在这里轮换备用 SSID ——
        // 那只是早期排查用的临时手段，已删。
        s_reconnect_tries++;
        uint32_t backoff_ms = (s_reconnect_tries <= RECONNECT_IMMEDIATE)
                                  ? RECONNECT_FAST_MS
                                  : RECONNECT_SLOW_MS;
        ESP_LOGI(TAG, "第 %d 次连续重连失败，%u ms 后重试",
                 s_reconnect_tries, (unsigned)backoff_ms);
        vTaskDelay(pdMS_TO_TICKS(backoff_ms));

        // ⚠️ 必须在这里**再查一次** s_scan_mode。
        // 上面那次检查是在睡眠**之前**做的：如果这 2 秒里用户敲了 `scan`，
        // 本任务醒来就会在扫描进行中调用 esp_wifi_connect()，把扫描打断
        // （驱动里连接优先级高于扫描，会 abort scan 并返回 ESP_ERR_WIFI_STATE）。
        // 这正是「scan 命令永远扫到 0 个 AP」的成因之一 —— 官方点名的
        // scan 饿死（scan starve to death）场景。
        if (s_scan_mode) {
            ESP_LOGI(TAG, "扫描模式：放弃本次重连");
            return;
        }
        esp_wifi_connect();
    } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        ip_event_got_ip_t *e = (ip_event_got_ip_t *)data;
        ESP_LOGI(TAG, "拿到 IP: " IPSTR, IP2STR(&e->ip_info.ip));

        // 连上了就把连续失败计数清零。
        // 不清零的后果：失败 6 次后计数器永远停在 6，之后网络恢复了、
        // 再断线时**第一次**重连就要等 5 秒 —— 退避变成了永久惩罚。
        // 用 GOT_IP 而不是 STA_CONNECTED：关联成功但 DHCP 拿不到 IP
        // 也算不上「连上了」，那种情况该继续按失败退避。
        s_reconnect_tries = 0;

        xEventGroupSetBits(s_net_eg, BIT_WIFI_UP);
    }
}

// 扫描一次并把结果打到日志。串口 `scan` 命令和启动自检都用它。
//   - 能看到自家 AP → 接收通路正常，问题在 AP 侧或安全配置
//   - 一个都看不到 → **先别下「射频坏」的结论**：连接中的扫描会被驱动直接
//     拒绝（返回 ESP_ERR_WIFI_STATE），重连循环还会把扫描打断成「0 条」
//     （官方点名的 scan starve to death）。本函数对这两种情况都做有限重试，
//     重试后仍为 0 才有诊断价值。
static void wifi_scan_log(void)
{
    //  驻留时间显式给出，别依赖默认值。
    //
    //  ⚠️ 更正：早先这里写过「`= {0}` 会让驱动按 0ms 扫描，恒扫到 0 个 AP」，
    //  那个说法是**错的**，已查证推翻。IDF 文档明确规定（station-scenarios.rst
    //  的 scan_time 表）：active.min=0 且 max=0 时，驱动按默认 120ms 驻留，
    //  不会做 0ms 扫描（头文件常量 WIFI_ACTIVE_SCAN_MAX_DEFAULT_TIME=120 佐证）。
    //  所以 `= {0}` 本身不会造成空结果，别再拿它当嫌疑犯 ——
    //  空结果的真正原因是「连接中扫描被驱动拒绝/饿死」，见 wifi_scan_now()。
    //
    //  显式写 100/300 仍值得：语义清楚，不受将来默认值变动影响。
    //  上限别超 1500ms（头文件警告：驻留过久会让已连接的 STA 掉线）。
    wifi_scan_config_t sc = {
        .ssid        = NULL,        // NULL = 扫所有 SSID
        .bssid       = NULL,
        .channel     = 0,           // 0 = 扫所有信道
        .show_hidden = true,
        .scan_type   = WIFI_SCAN_TYPE_ACTIVE,
        .scan_time.active.min = 100,
        .scan_time.active.max = 300,
    };

    // 最多试 3 次：连接中的扫描会被驱动拒绝，重连循环也会把扫描打断。
    // 这两种情况重试都能自愈；真正的「射频收不到任何东西」重试多少次都是 0，
    // 所以重试不会掩盖故障，只会把「被抢断」和「真没有」区分开。
    //
    // ⚠️ 必须区分两种「0 条」—— 它们的诊断含义完全相反：
    //   * 3 次都被驱动拒绝（ESP_ERR_WIFI_STATE）→ 本次**无效**，射频好坏未知；
    //   * 扫描真的跑完了却收到 0 个信标        → 这才指向射频/天线。
    // 早先的版本把两者都打成「3 次均为 0 个 AP，值得怀疑射频」，
    // 会把「重连抢断」误报成「射频坏」—— 正是上一轮误判的根源。
    uint16_t n = 0;
    int ran = 0;          // 真正跑完的扫描次数（区别于被驱动拒绝的次数）
    for (int attempt = 1; attempt <= 3; attempt++) {
        esp_err_t err = esp_wifi_scan_start(&sc, true);
        if (err == ESP_ERR_WIFI_STATE) {
            // 连接/重连过程中驱动不允许扫描（连接优先级高于扫描）。不是故障。
            ESP_LOGW(TAG, "扫描第 %d 次被拒：STA 正在连接中，500ms 后重试", attempt);
            vTaskDelay(pdMS_TO_TICKS(500));
            continue;
        }
        if (err != ESP_OK) {
            ESP_LOGW(TAG, "扫描启动失败: %s", esp_err_to_name(err));
            return;
        }

        ran++;
        n = 0;
        esp_wifi_scan_get_ap_num(&n);   // 必须先取数量，再取记录（IDF #13496）
        if (n > 0) break;

        // ESP_OK 但 0 条 = 扫描跑完了却没收到任何信标，通常是被重连打断。
        // 绝不能把这一轮当成「射频坏」的证据上报。
        ESP_LOGW(TAG, "扫描第 %d 次跑完但 0 个 AP（疑似被重连打断），500ms 后重试",
                 attempt);
        vTaskDelay(pdMS_TO_TICKS(500));
    }

    if (n == 0) {
        if (ran == 0) {
            ESP_LOGW(TAG, "=== 3 次扫描全被驱动拒绝（STA 一直在连接中）——"
                          " 本次结果无效，射频好坏未知，请稍后重试 ===");
        } else {
            ESP_LOGW(TAG, "=== 扫描实际跑完 %d 次，均为 0 个 AP ——"
                          " 到这一步才值得怀疑射频/天线 ===", ran);
        }
        return;
    }
    if (n > 20) n = 20;

    wifi_ap_record_t *recs = calloc(n, sizeof(wifi_ap_record_t));
    if (!recs) return;
    esp_wifi_scan_get_ap_records(&n, recs);

    ESP_LOGW(TAG, "=== 扫描到 %u 个 AP ===", (unsigned)n);
    for (int i = 0; i < n; i++) {
        ESP_LOGW(TAG, "  [%d] SSID='%s' ch=%u rssi=%d auth=%d",
                 i, (const char *)recs[i].ssid,
                 recs[i].primary, recs[i].rssi, recs[i].authmode);
    }
    free(recs);
}

// ---------------------------------------------------------------------------
//  wifi_ctrl.h 的实现 —— 供串口控制台调用
// ---------------------------------------------------------------------------
void wifi_reconnect_with_cfg(void)
{
    wifi_config_t wc;
    wifi_fill_config(&wc);

    esp_wifi_disconnect();
    esp_err_t err = esp_wifi_set_config(WIFI_IF_STA, &wc);
    if (err != ESP_OK) {
        ESP_LOGW(TAG, "写入 STA 配置失败: %s", esp_err_to_name(err));
        return;
    }
    ESP_LOGI(TAG, "按新配置重连: ssid='%s'", cfg_wifi_ssid());
    esp_wifi_connect();
}

void wifi_set_tx_power_now(int8_t dbm)
{
    esp_err_t err = esp_wifi_set_max_tx_power((int8_t)(dbm * 4));
    if (err != ESP_OK) {
        ESP_LOGW(TAG, "设置发射功率失败: %s", esp_err_to_name(err));
        return;
    }
    int8_t actual = 0;
    esp_wifi_get_max_tx_power(&actual);
    ESP_LOGI(TAG, "发射功率已改为 %d dBm（驱动实际 %d.%02d dBm）",
             (int)dbm, actual / 4, (actual % 4) * 25);
}

void wifi_scan_now(void)
{
    // 驱动不允许「连接中」扫描 —— STA 处于 connecting 状态时
    // esp_wifi_scan_start() 直接返回 ESP_ERR_WIFI_STATE，一个 AP 都扫不到。
    // 而断线重连又会在 2 秒后自动发起，扫描几乎永远撞在这个窗口里。
    //
    // 所以：先竖起 s_scan_mode 挡住自动重连（见 wifi_event_handler），
    // 断开，等驱动真正回到 idle，扫完再连回去。
    s_scan_mode = true;
    esp_wifi_disconnect();                  // 本来就断着的话会返回错误，无所谓
    vTaskDelay(pdMS_TO_TICKS(400));         // 给驱动时间真正停下来

    wifi_scan_log();

    s_scan_mode = false;
    esp_wifi_connect();
}

static void wifi_start(void)
{
    ESP_ERROR_CHECK(esp_netif_init());
    ESP_ERROR_CHECK(esp_event_loop_create_default());
    esp_netif_create_default_wifi_sta();

    wifi_init_config_t cfg = WIFI_INIT_CONFIG_DEFAULT();
    ESP_ERROR_CHECK(esp_wifi_init(&cfg));

    ESP_ERROR_CHECK(esp_event_handler_instance_register(
        WIFI_EVENT, ESP_EVENT_ANY_ID, &wifi_event_handler, NULL, NULL));
    ESP_ERROR_CHECK(esp_event_handler_instance_register(
        IP_EVENT, IP_EVENT_STA_GOT_IP, &wifi_event_handler, NULL, NULL));

    wifi_config_t wc;
    wifi_fill_config(&wc);

    ESP_ERROR_CHECK(esp_wifi_set_mode(WIFI_MODE_STA));
    ESP_ERROR_CHECK(esp_wifi_set_config(WIFI_IF_STA, &wc));
    ESP_ERROR_CHECK(esp_wifi_start());

    // 关掉 modem sleep（省电模式）。
    // 症状：关联成功 ~1 秒后被 AP 踢掉（assoc -> init, reason=4 ASSOC_EXPIRE），
    //       DHCP 永远拿不到 IP。有些 AP 在终端进入 modem sleep、错过它的
    //       keepalive 探测时就会主动踢人。关掉省电是最标准的解法。
    // 代价：待机电流大一点（约几十 mA），本设备常年通电，无所谓。
    ESP_ERROR_CHECK(esp_wifi_set_ps(WIFI_PS_NONE));

    // 发射功率必须等驱动起来之后才能设。
    wifi_apply_tx_power();

    ESP_LOGI(TAG, "等待连接 2.4GHz SSID: '%s'（密码%s）",
             cfg_wifi_ssid(), cfg_wifi_pass()[0] ? "已设置" : "为空 = 开放网络");

    // 这里**不要**再调 esp_wifi_connect()。
    // esp_wifi_start() 会触发 WIFI_EVENT_STA_START，那个回调里已经连了一次；
    // 这里再连一次，驱动会回 "sta is connecting, return error"，日志里多一行
    // 看着像故障的 error，实际无害但干扰排查。连接统一由 STA_START 回调发起。
}

// ============================================================================
//  SNTP 时间同步
//
//  C3 没有电池供电的 RTC，重启后时间归零。
//  在同步成功之前必须拒绝执行指令，否则 ts 校验形同虚设。
// ============================================================================
static void time_sync_task(void *arg)
{
    esp_sntp_config_t cfg = ESP_NETIF_SNTP_DEFAULT_CONFIG("pool.ntp.org");
    esp_netif_sntp_init(&cfg);

    ESP_LOGI(TAG, "等待 SNTP 时间同步…");
    if (esp_netif_sntp_sync_wait(pdMS_TO_TICKS(30000)) == ESP_OK) {
        time_t now = time(NULL);
        struct tm tm;
        localtime_r(&now, &tm);
        ESP_LOGI(TAG, "时间已同步: %04d-%02d-%02d %02d:%02d:%02d",
                 tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
                 tm.tm_hour, tm.tm_min, tm.tm_sec);
        cmd_set_time_synced(true);
        xEventGroupSetBits(s_net_eg, BIT_TIME_SYNC);
    } else {
        ESP_LOGE(TAG, "SNTP 同步失败 —— 指令将一律被拒绝（这是设计如此）");
    }

    esp_netif_sntp_deinit();
    vTaskDelete(NULL);
}

// ============================================================================
//  MQTT
// ============================================================================
static void publish_state(void)
{
    if (!s_mqtt_up) return;

#if ENABLE_STATUS_READBACK
    bool on = pc_state_is_on();
    char buf[64];
    snprintf(buf, sizeof(buf), "{\"pc\":\"%s\"}", on ? "on" : "off");
    esp_mqtt_client_publish(s_mqtt, s_topic_state, buf, 0, 1, 1);  // retained
#endif
}

static void publish_ack(const char *nonce, const char *result)
{
    if (!s_mqtt_up) return;
    char buf[128];
    snprintf(buf, sizeof(buf), "{\"nonce\":\"%s\",\"result\":\"%s\"}",
             nonce ? nonce : "", result);
    esp_mqtt_client_publish(s_mqtt, s_topic_ack, buf, 0, 1, 0);
}

static void mqtt_event_handler(void *arg, esp_event_base_t base,
                               int32_t event_id, void *event_data)
{
    esp_mqtt_event_handle_t e = (esp_mqtt_event_handle_t)event_data;

    switch ((esp_mqtt_event_id_t)event_id) {
    case MQTT_EVENT_CONNECTED:
        ESP_LOGI(TAG, "MQTT 已连接");
        s_mqtt_up = true;
        esp_mqtt_client_subscribe(s_mqtt, s_topic_cmd, 1);   // QoS 1
        // 遗嘱/在线状态：retained，手机端一订阅就能看到
        esp_mqtt_client_publish(s_mqtt, s_topic_status,
                                "{\"online\":true}", 0, 1, 1);
        publish_state();
        break;

    case MQTT_EVENT_DISCONNECTED:
        ESP_LOGW(TAG, "MQTT 断开");
        s_mqtt_up = false;
        break;

    case MQTT_EVENT_DATA: {
        // 注意：这里绝对不拉 GPIO。只做校验 + 入队，立即返回。
        ESP_LOGI(TAG, "收到消息 topic=%.*s len=%d",
                 e->topic_len, e->topic, e->data_len);

        uint16_t width = 0;
        int64_t now = cmd_time_synced() ? (int64_t)time(NULL) : 0;

        cmd_result_t r = cmd_verify(e->topic, e->topic_len,
                                    e->data, e->data_len, now, &width);

        if (r == CMD_OK) {
            if (pulse_request(width) == ESP_OK) {
                publish_ack(NULL, "accepted");
            } else {
                publish_ack(NULL, "busy");
            }
        } else {
            publish_ack(NULL, cmd_result_str(r));
        }
        break;
    }

    case MQTT_EVENT_ERROR:
        if (e->error_handle->error_type == MQTT_ERROR_TYPE_TCP_TRANSPORT) {
            ESP_LOGE(TAG, "传输层错误: esp-tls 0x%x, errno %d",
                     e->error_handle->esp_tls_last_esp_err,
                     e->error_handle->esp_transport_sock_errno);
        }
        break;

    default:
        break;
    }
}

static void mqtt_start(void)
{
    char uri[128];
    snprintf(uri, sizeof(uri), "mqtts://%s:%d", MQTT_HOST, MQTT_PORT);

    esp_mqtt_client_config_t cfg = {
        .broker.address.uri = uri,
        .credentials.username = MQTT_USER,
        .credentials.authentication.password = REDACTED_MQTT_PASS,
        // 校验服务器证书 —— 必须，不能跳过
        .broker.verification.crt_bundle_attach = esp_crt_bundle_attach,
        .session.keepalive = cfg_keepalive_s(),
        .session.last_will = {
            .topic = s_topic_status,
            .msg   = "{\"online\":false}",
            .msg_len = 17,
            .qos   = 1,
            .retain = 1,
        },
        .network.reconnect_timeout_ms = RECONNECT_BASE_MS,
        .network.disable_auto_reconnect = false,
    };

    s_mqtt = esp_mqtt_client_init(&cfg);
    ESP_ERROR_CHECK(esp_mqtt_client_register_event(
        s_mqtt, ESP_EVENT_ANY_ID, mqtt_event_handler, NULL));
    ESP_ERROR_CHECK(esp_mqtt_client_start(s_mqtt));
    ESP_LOGI(TAG, "MQTT 启动: %s", uri);
}

// ============================================================================
//  状态上报任务
// ============================================================================
static void status_task(void *arg)
{
    for (;;) {
        vTaskDelay(pdMS_TO_TICKS(cfg_status_period_s() * 1000));
        publish_state();
#if ENABLE_STATUS_READBACK
        ESP_LOGI(TAG, "PC 状态: %s", pc_state_is_on() ? "开着" : "关着");
#endif
    }
}

// ============================================================================
//  app_main
// ============================================================================
void app_main(void)
{
    ESP_LOGI(TAG, "=== powerswitch 启动 ===");

    // ------------------------------------------------------------------
    // 第 1 件事，凌驾于一切之上：把驱动光耦的 GPIO 钉在低电平。
    // 不等到 Wi-Fi、不等到 MQTT。哪怕后面全崩了，PC 也不受任何影响。
    // ------------------------------------------------------------------
    pulse_init();
#if ENABLE_STATUS_READBACK
    pc_state_init();
#endif

    // NVS（Wi-Fi 驱动和运行时配置都需要）
    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        ESP_ERROR_CHECK(nvs_flash_erase());
        err = nvs_flash_init();
    }
    ESP_ERROR_CHECK(err);

    // 载入运行时配置：NVS 里有就用 NVS 的，没有就用编译期默认值。
    // 必须早于 wifi_start()，因为 SSID / 密码 / 发射功率都从这里取。
    cfg_init();

    // 串口控制台：尽早起来，方便不进 monitor 也能改参数。
    // 失败也只是「改参数不方便」，不影响开关机功能。
    console_start();

    // device_id 由 MAC 派生：全球唯一、不含敏感信息
    uint8_t mac[6];
    ESP_ERROR_CHECK(esp_read_mac(mac, ESP_MAC_WIFI_STA));
    snprintf(s_device_id, sizeof(s_device_id), "psw-%02x%02x%02x",
             mac[3], mac[4], mac[5]);

    snprintf(s_topic_cmd,    sizeof(s_topic_cmd),    "psw/%s/cmd",    s_device_id);
    snprintf(s_topic_state,  sizeof(s_topic_state),  "psw/%s/state",  s_device_id);
    snprintf(s_topic_status, sizeof(s_topic_status), "psw/%s/status", s_device_id);
    snprintf(s_topic_ack,    sizeof(s_topic_ack),    "psw/%s/ack",    s_device_id);

    cmd_verify_init(s_device_id);
    ESP_LOGI(TAG, "device_id = %s", s_device_id);

    s_net_eg = xEventGroupCreate();

    // 看门狗：安全相关项。防 GPIO 卡高导致主板强制断电。
    esp_task_wdt_config_t wdt = {
        .timeout_ms = 30000,
        .idle_core_mask = 0,
        .trigger_panic = true,
    };
    esp_task_wdt_reconfigure(&wdt);
    esp_task_wdt_add(NULL);     // 监控 app_main 自身

    wifi_start();

    // 等 Wi-Fi 连上。
    // ⚠️ 这里不能再用 portMAX_DELAY 一次性等待：app_main 已被挂进 30 秒看门狗
    // （见上面的 esp_task_wdt_add），一旦路由器连不上，阻塞超过 30 秒就会被
    // 看门狗判定为死锁 -> trigger_panic -> 重启，于是陷入「重启-连不上-重启」
    // 的死循环，把真正的问题（连不上路由器）掩盖掉。
    // 改成循环等待，每秒喂一次狗；连不上就一直等，wifi 事件回调会每 2 秒重连。
    // 期间串口 `wifi <ssid> <pass>` 可以随时改配置，不用重新烧录。
    while (!(xEventGroupGetBits(s_net_eg) & BIT_WIFI_UP)) {
        esp_task_wdt_reset();
        vTaskDelay(pdMS_TO_TICKS(1000));
    }
    ESP_LOGI(TAG, "Wi-Fi 已就绪，继续初始化 MQTT");

    xTaskCreate(time_sync_task, "sntp", 4096, NULL, 4, NULL);
    xTaskCreate(status_task,    "status", 3072, NULL, 3, NULL);

    mqtt_start();

    ESP_LOGI(TAG, "初始化完毕，等待指令");
    ESP_LOGI(TAG, "主题: %s", s_topic_cmd);

    for (;;) {
        esp_task_wdt_reset();
        vTaskDelay(pdMS_TO_TICKS(5000));
    }
}
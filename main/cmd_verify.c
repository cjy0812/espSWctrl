#include "cmd_verify.h"
#include "app_config.h"
#include "cfg.h"

#include <string.h>
#include <stdio.h>
#include <stdlib.h>

#include "cJSON.h"
#include "esp_log.h"
#include "esp_timer.h"

static const char *TAG = "cmd";

static char     s_expected_topic[96];   // "psw/<device_id>/cmd"
static bool     s_time_synced;

// ---- nonce 去重：环形缓冲，存 FNV-1a 32 位哈希 ----
// QoS 1 语义是"至少一次"，broker 抖动时会把同一条消息发两遍。
// 没有去重，你自己的网络抖动就会让电脑被多按一下。
#define NONCE_SLOTS 32
static uint32_t s_nonce_ring[NONCE_SLOTS];
static int      s_nonce_idx;

static uint32_t fnv1a(const char *s)
{
    uint32_t h = 2166136261u;
    while (*s) {
        h ^= (uint8_t)*s++;
        h *= 16777619u;
    }
    return h;
}

static bool nonce_seen(uint32_t h)
{
    for (int i = 0; i < NONCE_SLOTS; i++) {
        if (s_nonce_ring[i] == h) return true;
    }
    return false;
}

static void nonce_remember(uint32_t h)
{
    s_nonce_ring[s_nonce_idx] = h;
    s_nonce_idx = (s_nonce_idx + 1) % NONCE_SLOTS;
}

void cmd_verify_init(const char *device_id)
{
    snprintf(s_expected_topic, sizeof(s_expected_topic),
             "psw/%s/cmd", device_id);
    memset(s_nonce_ring, 0, sizeof(s_nonce_ring));
    s_nonce_idx = 0;
    s_time_synced = false;
    ESP_LOGI(TAG, "只接受 topic: %s", s_expected_topic);
}

void cmd_set_time_synced(bool synced) { s_time_synced = synced; }
bool cmd_time_synced(void)            { return s_time_synced; }

const char *cmd_result_str(cmd_result_t r)
{
    switch (r) {
    case CMD_OK:            return "ok";
    case CMD_ERR_JSON:      return "bad-json";
    case CMD_ERR_SCHEMA:    return "bad-schema";
    case CMD_ERR_UNKNOWN:   return "unknown-cmd";
    case CMD_ERR_EXPIRED:   return "expired";
    case CMD_ERR_REPLAY:    return "replay";
    case CMD_ERR_NOT_SYNCED:return "time-not-synced";
    }
    return "?";
}

// ---------------------------------------------------------------------------
//  纵深防御第一层：topic 必须逐字节等于我们期望的那一个
// ---------------------------------------------------------------------------
static bool topic_matches(const char *topic, int topic_len)
{
    size_t want = strlen(s_expected_topic);
    if (topic_len != (int)want) return false;
    return memcmp(topic, s_expected_topic, want) == 0;
}

cmd_result_t cmd_verify(const char *topic, int topic_len,
                        const char *data, int data_len,
                        int64_t now_ts, uint16_t *out_width_ms)
{
    // --- 1. topic 白名单（逐字节）---
    if (!topic_matches(topic, topic_len)) {
        ESP_LOGW(TAG, "topic 不匹配，丢弃：%.*s", topic_len, topic);
        return CMD_ERR_SCHEMA;
    }

    // --- 2. 时间必须先同步，否则 ts 校验形同虚设 ---
    if (!s_time_synced) {
        ESP_LOGW(TAG, "时间未同步，拒绝执行");
        return CMD_ERR_NOT_SYNCED;
    }

    // --- 3. 解析 JSON（cJSON 要求以 NUL 结尾，这里拷一份）---
    char *buf = malloc(data_len + 1);
    if (!buf) return CMD_ERR_JSON;
    memcpy(buf, data, data_len);
    buf[data_len] = '\0';

    cJSON *root = cJSON_Parse(buf);
    free(buf);
    if (!root) {
        ESP_LOGW(TAG, "JSON 解析失败");
        return CMD_ERR_JSON;
    }

    cmd_result_t ret = CMD_ERR_SCHEMA;
    cJSON *j_cmd   = cJSON_GetObjectItem(root, "cmd");
    cJSON *j_nonce = cJSON_GetObjectItem(root, "nonce");
    cJSON *j_ts    = cJSON_GetObjectItem(root, "ts");
    cJSON *j_ttl   = cJSON_GetObjectItem(root, "ttl");

    if (!cJSON_IsString(j_cmd) || !cJSON_IsString(j_nonce) ||
        !cJSON_IsNumber(j_ts)) {
        ESP_LOGW(TAG, "字段缺失或类型不对");
        goto done;
    }

    // --- 4. 指令语义：只有 press / press_long ---
    // 注意：绝不提供 on/off —— 电源键是"按一下翻转"，不是开关。
    uint16_t width = cfg_pulse_ms();
    if (strcmp(j_cmd->valuestring, "press") == 0) {
        width = cfg_pulse_ms();
    } else if (strcmp(j_cmd->valuestring, "press_long") == 0) {
        width = 5000;   // 会被 pulse.c 夹到 PULSE_MS_MAX(3000)
    } else {
        ESP_LOGW(TAG, "未知指令: %s", j_cmd->valuestring);
        ret = CMD_ERR_UNKNOWN;
        goto done;
    }

    // --- 5. 时间窗 ---
    int ttl = cJSON_IsNumber(j_ttl) ? (int)j_ttl->valuedouble : cfg_cmd_ttl();
    if (ttl <= 0 || ttl > 300) ttl = cfg_cmd_ttl();

    int64_t ts = (int64_t)j_ts->valuedouble;
    int64_t skew = now_ts - ts;
    if (skew < 0) skew = -skew;
    if (skew > ttl) {
        ESP_LOGW(TAG, "指令过期：偏移 %lld 秒 > %d", (long long)skew, ttl);
        ret = CMD_ERR_EXPIRED;
        goto done;
    }

    // --- 6. nonce 去重 ---
    uint32_t h = fnv1a(j_nonce->valuestring);
    if (nonce_seen(h)) {
        ESP_LOGW(TAG, "nonce 重复，丢弃（防重放 / 防 QoS1 重投）");
        ret = CMD_ERR_REPLAY;
        goto done;
    }
    nonce_remember(h);

    if (out_width_ms) *out_width_ms = width;
    ret = CMD_OK;

done:
    cJSON_Delete(root);
    return ret;
}
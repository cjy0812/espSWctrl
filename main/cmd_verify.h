#pragma once

#include <stdbool.h>
#include <stdint.h>
#include "esp_err.h"

// ============================================================================
//  指令校验：解析 JSON + 防重放 + topic 逐字节自校验
//
//  为什么要在固件里做 topic 校验（架构文档 4.4）：
//   EMQX Serverless 可能不开放细粒度 ACL。不能假设上游一定配对了，
//   收到消息先检查 topic 是否恰好等于 "psw/<自己的device_id>/cmd"，
//   不等就丢弃。几行代码，把"broker 配置不当"这个风险从架构里消掉。
//  这叫纵深防御。
// ============================================================================

typedef enum {
    CMD_OK = 0,
    CMD_ERR_JSON,       // JSON 解析失败
    CMD_ERR_SCHEMA,     // 字段缺失/类型不对
    CMD_ERR_UNKNOWN,    // cmd 不是已知指令
    CMD_ERR_EXPIRED,    // ts + ttl 超窗
    CMD_ERR_REPLAY,     // nonce 重复
    CMD_ERR_NOT_SYNCED, // 时间未同步，拒绝执行
} cmd_result_t;

// 初始化：记录本机 device_id（由 MAC 派生），并清空 nonce 历史
void cmd_verify_init(const char *device_id);

// 校验一条 MQTT 下行消息。
//   topic / topic_len  : broker 给的原始 topic（不是以 '\0' 结尾）
//   data  / data_len   : 载荷
//   now_ts             : 当前 Unix 时间（秒），未同步时传 0
// 通过校验则把建议的脉冲宽度写入 *out_width_ms。
cmd_result_t cmd_verify(const char *topic, int topic_len,
                        const char *data, int data_len,
                        int64_t now_ts, uint16_t *out_width_ms);

// 把校验结果转成人话（用于日志和 ack）
const char *cmd_result_str(cmd_result_t r);

// 时间是否已同步（SNTP）。未同步前一律拒绝执行指令。
void cmd_set_time_synced(bool synced);
bool cmd_time_synced(void);
#include "pulse.h"
#include "app_config.h"
#include "cfg.h"

#include "driver/gpio.h"
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/queue.h"

static const char *TAG = "pulse";

static QueueHandle_t s_queue;
static volatile bool s_busy;

// ---------------------------------------------------------------------------
// 脉冲任务：串行执行，独立于 MQTT 回调
//
// 关键：这里只做「拉高 → 精确延时 → 拉低」，中间不等待网络、不取锁。
//      看门狗监控本任务 —— 一旦死锁，GPIO 卡高超过 4 秒会强制关机，
//      所以 WDT 在本项目是安全相关项，不是可选项。
// ---------------------------------------------------------------------------
static void pulse_task(void *arg)
{
    uint16_t width_ms;

    for (;;) {
        if (xQueueReceive(s_queue, &width_ms, portMAX_DELAY) != pdTRUE) {
            continue;
        }

        // 双保险：夹到安全区间
        if (width_ms < PULSE_MS_MIN) width_ms = PULSE_MS_MIN;
        if (width_ms > PULSE_MS_MAX) width_ms = PULSE_MS_MAX;

        s_busy = true;
        ESP_LOGI(TAG, "按下电源键：脉冲 %u ms", (unsigned)width_ms);

        // ---- 唯一操作 GPIO 的地方 ----
        gpio_set_level(PIN_PWR_SW, 1);
        vTaskDelay(pdMS_TO_TICKS(width_ms));
        gpio_set_level(PIN_PWR_SW, 0);
        // -----------------------------

        s_busy = false;

        // 两次脉冲之间留足间隔，防被主板识别为长按/组合键
        vTaskDelay(pdMS_TO_TICKS(PULSE_GAP_MIN_MS));
    }
}

void pulse_init(void)
{
    // 1) 第一件事：把 GPIO5 拉低，绝不等到 Wi-Fi/MQTT 连上。
    //    复位后 GPIO 是高阻（Hi-Z），光耦不导通，PC 本来就不受影响；
    //    这里显式置低是把它钉死，防止周围噪声感应出电压。
    gpio_config_t cfg = {
        .pin_bit_mask = (1ULL << PIN_PWR_SW),
        .mode         = GPIO_MODE_OUTPUT,
        .pull_up_en   = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_ENABLE,   // 对应电路里的 R2 10k 下拉
        .intr_type    = GPIO_INTR_DISABLE,
    };
    ESP_ERROR_CHECK(gpio_config(&cfg));
    gpio_set_level(PIN_PWR_SW, 0);

    s_queue = xQueueCreate(4, sizeof(uint16_t));
    configASSERT(s_queue);

    xTaskCreate(pulse_task, "pulse", 3072, NULL, 6, NULL);
    ESP_LOGI(TAG, "初始化完成，GPIO%d 已置低（安全态）", PIN_PWR_SW);
}

esp_err_t pulse_request(uint16_t width_ms)
{
    // 0 表示"用当前配置的默认宽度"（NVS 里存的值，串口可改）
    if (width_ms == 0) width_ms = cfg_pulse_ms();
    if (xQueueSend(s_queue, &width_ms, 0) != pdTRUE) {
        ESP_LOGW(TAG, "脉冲队列已满，拒绝请求");
        return ESP_ERR_TIMEOUT;
    }
    return ESP_OK;
}

bool pulse_is_busy(void)
{
    return s_busy;
}
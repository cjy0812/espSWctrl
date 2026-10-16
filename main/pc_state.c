#include "pc_state.h"
#include "app_config.h"

#include "driver/gpio.h"
#include "esp_log.h"

static const char *TAG = "pc_state";

void pc_state_init(void)
{
    gpio_config_t cfg = {
        .pin_bit_mask = (1ULL << PIN_PC_STATE),
        .mode         = GPIO_MODE_INPUT,
        .pull_up_en   = GPIO_PULLUP_ENABLE,     // 光耦截止时把电平钉在高
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type    = GPIO_INTR_DISABLE,
    };
    ESP_ERROR_CHECK(gpio_config(&cfg));
    ESP_LOGI(TAG, "初始化完成，GPIO%d 作为状态回读输入", PIN_PC_STATE);
}

bool pc_state_is_on(void)
{
    // 低电平 = 光耦导通 = SATA 有 5V = PC 开着
    return gpio_get_level(PIN_PC_STATE) == 0;
}
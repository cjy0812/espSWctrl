idf_component_register(
    SRCS "main.c"
         "pulse.c"
         "pc_state.c"
         "cmd_verify.c"
         "cfg.c"
         "console_cmds.c"
    INCLUDE_DIRS "."
    PRIV_REQUIRES
        esp_wifi
        esp_event
        esp_netif
        nvs_flash
        esp-tls
        mqtt
        cjson
        esp_driver_gpio
        esp_timer
        mbedtls
        console
)
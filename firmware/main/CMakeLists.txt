idf_component_register(
    SRCS
        "main.c"
        "report.c"
        "efuse_report.c"
        "storage_key.c"
        "ui_board_test.c"
        "ui_storage_key.c"
        "board/board.c"
    # The components from main/idf_component.yml (the board package, LVGL and the drivers) are
    # added as requirements by the component manager.
    PRIV_REQUIRES app_update esp_app_format log efuse bootloader_support heap esp_driver_i2c
                  esp_lcd esp_hw_support esp_partition esp_system hal nvs_flash nvs_sec_provider
)

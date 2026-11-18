idf_component_register(
    SRCS
        "main.c"
        "report.c"
        "efuse_report.c"
        "ui_board_test.c"
        "board/board.c"
    # The components from main/idf_component.yml (the board package, LVGL and the drivers) are
    # added as requirements by the component manager.
    PRIV_REQUIRES app_update esp_app_format log efuse bootloader_support heap esp_driver_i2c
                  esp_lcd
)

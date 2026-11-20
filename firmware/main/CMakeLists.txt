idf_component_register(
    SRCS
        "main.c"
        "report.c"
        "efuse_report.c"
        "storage_key.c"
        "storage.c"
        "usb_setup.c"
        "wifi.c"
        "hub_http.c"
        "hub.c"
        "reset_button.c"
        "ui.c"
        "ui_font.c"
        "ui_page.c"
        "ui_text.c"
        "board/board.c"
    # Geist, the desktop app's font, for the screens (ui_font.c; licence in THIRD_PARTY.md).
    EMBED_FILES
        "../../assets/fonts/Geist-Regular.ttf"
        "../../assets/fonts/Geist-SemiBold.ttf"
    # The components from main/idf_component.yml (the board package, LVGL and the drivers) are
    # added as requirements by the component manager.
    PRIV_REQUIRES app_update esp_app_format log efuse bootloader_support heap esp_driver_i2c
                  esp_lcd esp_hw_support esp_partition esp_system hal nvs_flash nvs_sec_provider
                  esp_driver_usb_serial_jtag esp_driver_gpio esp_timer esp_wifi esp_netif
                  esp_event esp_http_client esp-tls mbedtls dh_core
)

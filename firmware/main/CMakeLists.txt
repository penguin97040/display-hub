idf_component_register(
    SRCS
        "main.c"
        "report.c"
        "efuse_report.c"
        "storage_key.c"
        "storage.c"
        "storage_fw.c"
        "usb_setup.c"
        "wifi.c"
        "hub_http.c"
        "hub.c"
        "update.c"
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
                  esp_event esp_http_client esp-tls tcp_transport mbedtls dh_core
)

# TEST ONLY. A build made with -DDH_TEST_TRIAL=no-check-in or -DDH_TEST_TRIAL=crash fails its
# trial boot on purpose, to try the rollback on a board (README.md, "Test builds"). The top-level
# CMakeLists.txt refuses it unless the version carries a pre-release tag, so it can never be
# built as a release.
if(DH_TEST_TRIAL STREQUAL "no-check-in")
    target_compile_definitions(${COMPONENT_LIB} PRIVATE DH_TEST_TRIAL_NO_CHECK_IN=1)
elseif(DH_TEST_TRIAL STREQUAL "crash")
    target_compile_definitions(${COMPONENT_LIB} PRIVATE DH_TEST_TRIAL_CRASH=1)
endif()

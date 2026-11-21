get_filename_component(FW "${CMAKE_CURRENT_LIST_DIR}/../../../.." ABSOLUTE)
idf_component_register(SRCS "harness.c" "${FW}/main/hub_http.c"
                       INCLUDE_DIRS "${FW}/main" "${FW}/components/dh_core/include"
                       PRIV_REQUIRES esp_http_client esp-tls tcp_transport mbedtls esp_timer log)

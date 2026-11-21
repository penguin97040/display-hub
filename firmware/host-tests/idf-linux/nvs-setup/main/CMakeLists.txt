idf_component_register(SRCS "harness.c" WHOLE_ARCHIVE REQUIRES nvs_flash
                       PRIV_REQUIRES spi_flash esp_partition)

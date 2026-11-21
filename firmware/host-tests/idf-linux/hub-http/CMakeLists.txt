# main/hub_http.c, built for a computer (ESP-IDF's Linux target) over ESP-IDF's own HTTP client,
# esp-tls and Mbed TLS. See ../README.md; run.sh builds and runs it.
cmake_minimum_required(VERSION 3.16)
# The full sdkconfig goes in the build folder, never beside the sources.
set(SDKCONFIG "${CMAKE_BINARY_DIR}/sdkconfig")
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
set(COMPONENTS main)
project(hub_http_harness)

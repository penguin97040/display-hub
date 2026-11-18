# The official Display Hub firmware, for the Waveshare ESP32-S3-Touch-AMOLED-1.8 (V2).
# Built with ESP-IDF v5.5.6 exactly: see README.md.
cmake_minimum_required(VERSION 3.16)

# The firmware version. It lands in the image's esp_app_desc_t, and `hub-firmware sign` checks the
# package's version against it.
set(PROJECT_VER "0.1.0")

# Only ESP-IDF's own components and main are looked for. Without this, ESP-IDF also reads every
# folder under components/, so a component that is still being written there could break this build.
# Add a component to this list when main starts to use it.
set(COMPONENT_DIRS "$ENV{IDF_PATH}/components" "${CMAKE_CURRENT_LIST_DIR}/main")

# Keep the generated sdkconfig in the build folder, so sdkconfig.defaults stays the only record of
# the settings and a build never writes into the source tree.
set(SDKCONFIG "${CMAKE_BINARY_DIR}/sdkconfig")

include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(display-hub)

# The official Display Hub firmware, for the Waveshare ESP32-S3-Touch-AMOLED-1.8 (V2).
# Built with ESP-IDF v5.5.6 exactly: see README.md.
cmake_minimum_required(VERSION 3.16)

# The firmware version. It lands in the image's esp_app_desc_t, and `hub-firmware sign` checks the
# package's version against it.
set(PROJECT_VER "0.1.0")

# TEST ONLY: two switches for trying updates on a board, both off unless given on the command
# line (README.md, "Test builds"). They are kept in the build folder's cache, so use a build
# folder of its own for a test build and never make a release from it.
#   -DDH_TEST_VERSION=0.1.0-dev.8   builds the same code as another version, so one build can be
#                                   offered as an update to another. It must carry a pre-release
#                                   tag ("-" and a tag), which makes it older than the release.
#   -DDH_TEST_TRIAL=no-check-in     firmware whose trial boot never counts an answer from the hub,
#   -DDH_TEST_TRIAL=crash           or crashes 20 seconds into it: both must be rolled back.
set(DH_TEST_VERSION "" CACHE STRING "TEST ONLY: a pre-release version to build as")
set(DH_TEST_TRIAL "" CACHE STRING "TEST ONLY: no-check-in or crash, to fail the trial boot")
if(NOT DH_TEST_VERSION STREQUAL "")
    if(NOT DH_TEST_VERSION MATCHES "^[0-9]+\\.[0-9]+\\.[0-9]+-[0-9A-Za-z.-]+$")
        message(FATAL_ERROR "DH_TEST_VERSION must be a pre-release version such as 0.1.0-dev.8")
    endif()
    set(PROJECT_VER "${DH_TEST_VERSION}")
    message(WARNING "TEST BUILD: version ${PROJECT_VER}. Not for release.")
endif()
if(NOT DH_TEST_TRIAL STREQUAL "")
    if(NOT DH_TEST_TRIAL MATCHES "^(no-check-in|crash)$")
        message(FATAL_ERROR "DH_TEST_TRIAL must be no-check-in or crash")
    endif()
    if(DH_TEST_VERSION STREQUAL "")
        message(FATAL_ERROR "DH_TEST_TRIAL needs DH_TEST_VERSION: test code never carries a "
                            "release version")
    endif()
    message(WARNING "TEST BUILD: this firmware fails its trial boot on purpose (${DH_TEST_TRIAL}).")
endif()

# Only ESP-IDF's own components, main and the components named here are looked for. Without this,
# ESP-IDF also reads every folder under components/, so a component that is still being written
# there could break this build. Add a component to this list when main starts to use it.
set(COMPONENT_DIRS "$ENV{IDF_PATH}/components" "${CMAKE_CURRENT_LIST_DIR}/main"
                   "${CMAKE_CURRENT_LIST_DIR}/components/dh_core")

# Keep the generated sdkconfig in the build folder, so sdkconfig.defaults stays the only record of
# the settings and a build never writes into the source tree.
set(SDKCONFIG "${CMAKE_BINARY_DIR}/sdkconfig")

include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(display-hub)

# Host tests for dh_core. CI runs:
#   cmake -S firmware/host-tests -B <dir> -DMBEDTLS_DIR=<prefix>
#   cmake --build <dir>
#   ctest --test-dir <dir> --output-on-failure
#
# MBEDTLS_DIR is a built Mbed TLS 3.6 source tree or an install prefix; without it, the system's
# Mbed TLS is used. The signed-update core needs it for SHA-256 and ECDSA P-256.
#
# -DDH_SANITIZE=ON builds everything with AddressSanitizer and UndefinedBehaviorSanitizer, and
# -DCMAKE_C_COMPILER picks gcc or clang. The vector and example tests need Python 3 (standard
# library only).
cmake_minimum_required(VERSION 3.16)
project(dh_host_tests C)

# Where Mbed TLS is: see components/dh_core/CMakeLists.txt.
set(MBEDTLS_DIR "" CACHE PATH "A built Mbed TLS 3.6 source tree, or its install prefix")

option(DH_WERROR "Treat compiler warnings as errors" ON)
option(DH_SANITIZE "Build with AddressSanitizer and UndefinedBehaviorSanitizer" OFF)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_C_EXTENSIONS OFF)

set(DH_WARNINGS -Wall -Wextra -Wpedantic -Wconversion -Wshadow -Wstrict-prototypes)
if(DH_WERROR)
    list(APPEND DH_WARNINGS -Werror)
endif()
if(DH_SANITIZE)
    add_compile_options(-fsanitize=address,undefined -fno-sanitize-recover=all -fno-omit-frame-pointer)
    add_link_options(-fsanitize=address,undefined)
endif()

get_filename_component(DH_FIRMWARE_DIR "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)
get_filename_component(DH_PROTOCOL_DIR "${DH_FIRMWARE_DIR}/../protocol/v1" ABSOLUTE)

add_subdirectory("${DH_FIRMWARE_DIR}/components/dh_core" dh_core)
target_compile_options(dh_core PRIVATE ${DH_WARNINGS})

add_executable(dh_unit_tests unit_tests.c)
target_link_libraries(dh_unit_tests PRIVATE dh_core)
target_compile_options(dh_unit_tests PRIVATE ${DH_WARNINGS})

add_executable(dh_conform conform_cli.c)
target_link_libraries(dh_conform PRIVATE dh_core)
target_compile_options(dh_conform PRIVATE ${DH_WARNINGS})

add_executable(dh_update_tests update_tests.c)
target_link_libraries(dh_update_tests PRIVATE dh_core)
target_compile_options(dh_update_tests PRIVATE ${DH_WARNINGS})

add_executable(dh_usb_tests usb_tests.c)
target_link_libraries(dh_usb_tests PRIVATE dh_core)
target_compile_options(dh_usb_tests PRIVATE ${DH_WARNINGS})
target_compile_definitions(dh_usb_tests PRIVATE DH_PROTOCOL_DIR="${DH_PROTOCOL_DIR}")

add_executable(dh_hub_tests hub_tests.c)
target_link_libraries(dh_hub_tests PRIVATE dh_core)
target_compile_options(dh_hub_tests PRIVATE ${DH_WARNINGS})
target_compile_definitions(dh_hub_tests PRIVATE DH_PROTOCOL_DIR="${DH_PROTOCOL_DIR}")

add_executable(dh_fw_conform fw_conform_cli.c)
target_link_libraries(dh_fw_conform PRIVATE dh_core)
target_compile_options(dh_fw_conform PRIVATE ${DH_WARNINGS})

# The USB handler on a pseudo-terminal, for trying the desktop app's sender against it (see
# usb-e2e/). Linux only, and not part of the tests run here.
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    add_executable(dh_usb_pty_display usb_pty_display.c)
    target_link_libraries(dh_usb_pty_display PRIVATE dh_core)
    target_compile_options(dh_usb_pty_display PRIVATE ${DH_WARNINGS})
endif()

# The fuzz targets, built with a small main that replays a folder of inputs.
set(DH_FUZZ_TARGETS json snapshot fw_manifest fw_download usb hub)
foreach(target ${DH_FUZZ_TARGETS})
    add_executable(dh_fuzz_${target}_replay
        "${DH_FIRMWARE_DIR}/fuzz/fuzz_${target}.c"
        "${DH_FIRMWARE_DIR}/fuzz/replay_main.c")
    target_link_libraries(dh_fuzz_${target}_replay PRIVATE dh_core)
    target_compile_options(dh_fuzz_${target}_replay PRIVATE ${DH_WARNINGS})
endforeach()

find_package(Python3 COMPONENTS Interpreter REQUIRED)

enable_testing()
add_test(NAME unit COMMAND dh_unit_tests)
add_test(NAME vectors
    COMMAND Python3::Interpreter -I "${CMAKE_CURRENT_LIST_DIR}/run_conformance.py"
            vectors $<TARGET_FILE:dh_conform> "${DH_PROTOCOL_DIR}/vectors")
add_test(NAME update COMMAND dh_update_tests)
add_test(NAME usb COMMAND dh_usb_tests)
add_test(NAME hub COMMAND dh_hub_tests)
add_test(NAME firmware_vectors
    COMMAND Python3::Interpreter -I "${CMAKE_CURRENT_LIST_DIR}/run_conformance.py"
            firmware $<TARGET_FILE:dh_fw_conform> "${DH_PROTOCOL_DIR}/firmware")
add_test(NAME examples
    COMMAND Python3::Interpreter -I "${CMAKE_CURRENT_LIST_DIR}/run_conformance.py"
            examples $<TARGET_FILE:dh_conform> "${DH_PROTOCOL_DIR}/examples")
foreach(target ${DH_FUZZ_TARGETS})
    add_test(NAME fuzz_${target}_corpus
        COMMAND dh_fuzz_${target}_replay "${DH_FIRMWARE_DIR}/fuzz/corpus/${target}")
endforeach()

# dh_core: the display's protocol and signed-update core, in plain C11 with no hardware. It builds
# as an ESP-IDF component, and as a plain library for the host tests (firmware/host-tests).
#
# The signed-update core (dh_crypto, dh_update_*) uses Mbed TLS 3.6 for SHA-256 and ECDSA P-256.
# On the chip that is ESP-IDF's mbedtls component. On a computer, MBEDTLS_DIR names either a built
# Mbed TLS source tree (headers in include/, libraries in library/) or an install prefix (include/
# and lib/); left empty, the system's Mbed TLS is used.
set(DH_CORE_SRCS
    src/dh_crypto.c
    src/dh_hub.c
    src/dh_json.c
    src/dh_poll.c
    src/dh_setup.c
    src/dh_snapshot.c
    src/dh_text.c
    src/dh_time.c
    src/dh_unicode_tables.c
    src/dh_update_package.c
    src/dh_update_state.c
    src/dh_usb.c
    src/dh_usb_line.c
)

if(ESP_PLATFORM)
    idf_component_register(SRCS ${DH_CORE_SRCS} INCLUDE_DIRS include REQUIRES mbedtls)
else()
    list(TRANSFORM DH_CORE_SRCS PREPEND "${CMAKE_CURRENT_LIST_DIR}/")
    add_library(dh_core STATIC ${DH_CORE_SRCS})
    target_include_directories(dh_core PUBLIC "${CMAKE_CURRENT_LIST_DIR}/include")
    target_compile_features(dh_core PUBLIC c_std_11)
    set_target_properties(dh_core PROPERTIES C_EXTENSIONS OFF)
    if(NOT APPLE AND NOT WIN32)
        target_link_libraries(dh_core PUBLIC m)
    endif()

    if(MBEDTLS_DIR)
        find_library(DH_MBEDCRYPTO mbedcrypto
            PATHS "${MBEDTLS_DIR}/library" "${MBEDTLS_DIR}/lib" NO_DEFAULT_PATH)
        if(NOT EXISTS "${MBEDTLS_DIR}/include/mbedtls/ecdsa.h")
            message(FATAL_ERROR "No Mbed TLS headers in ${MBEDTLS_DIR}/include")
        endif()
        # As system headers, so this project's warnings aren't applied to Mbed TLS's own.
        target_include_directories(dh_core SYSTEM PUBLIC "${MBEDTLS_DIR}/include")
    else()
        find_library(DH_MBEDCRYPTO mbedcrypto)
    endif()
    if(NOT DH_MBEDCRYPTO)
        message(FATAL_ERROR "Mbed TLS's libmbedcrypto wasn't found. Set -DMBEDTLS_DIR=<a built "
                            "Mbed TLS 3.6 source tree or install prefix>.")
    endif()
    target_link_libraries(dh_core PUBLIC "${DH_MBEDCRYPTO}")
endif()

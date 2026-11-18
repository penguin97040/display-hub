# dh_core: the display's protocol core, in plain C11 with no hardware. It builds as an ESP-IDF
# component, and as a plain library for the host tests (firmware/host-tests).
set(DH_CORE_SRCS
    src/dh_json.c
    src/dh_poll.c
    src/dh_snapshot.c
    src/dh_text.c
    src/dh_time.c
    src/dh_unicode_tables.c
    src/dh_usb_line.c
)

if(ESP_PLATFORM)
    idf_component_register(SRCS ${DH_CORE_SRCS} INCLUDE_DIRS include)
else()
    list(TRANSFORM DH_CORE_SRCS PREPEND "${CMAKE_CURRENT_LIST_DIR}/")
    add_library(dh_core STATIC ${DH_CORE_SRCS})
    target_include_directories(dh_core PUBLIC "${CMAKE_CURRENT_LIST_DIR}/include")
    target_compile_features(dh_core PUBLIC c_std_11)
    set_target_properties(dh_core PROPERTIES C_EXTENSIONS OFF)
    if(NOT APPLE AND NOT WIN32)
        target_link_libraries(dh_core PUBLIC m)
    endif()
endif()

// The official Display Hub firmware. For now it only starts up and says which version it is and
// where it is running from.

#include <inttypes.h>

#include "esp_app_desc.h"
#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"

static const char *TAG = "display-hub";

void app_main(void)
{
    const esp_app_desc_t *app = esp_app_get_description();
    ESP_LOGI(TAG, "display-hub firmware %s", app->version);

    const esp_partition_t *running = esp_ota_get_running_partition();
    if (running != NULL) {
        ESP_LOGI(TAG, "running from %s at 0x%08" PRIx32 " (%" PRIu32 " bytes)", running->label,
                 running->address, running->size);
    } else {
        ESP_LOGW(TAG, "couldn't tell which partition is running");
    }
}

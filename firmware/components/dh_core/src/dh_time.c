#include "dh_time.h"

static bool two(const char *s, int *out)
{
    if (s[0] < '0' || s[0] > '9' || s[1] < '0' || s[1] > '9') {
        return false;
    }
    *out = (s[0] - '0') * 10 + (s[1] - '0');
    return true;
}

static bool is_leap(int y)
{
    return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
}

/* Days from 1970-01-01 to y-m-d (Howard Hinnant's days_from_civil). */
static int64_t days_from_civil(int64_t y, int m, int d)
{
    y -= m <= 2;
    int64_t era = (y >= 0 ? y : y - 399) / 400;
    int64_t yoe = y - era * 400;
    int64_t mp = (m + 9) % 12;
    int64_t doy = (153 * mp + 2) / 5 + d - 1;
    int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return era * 146097 + doe - 719468;
}

bool dh_time_parse(const char *s, size_t n, int64_t *unix_seconds)
{
    static const int month_days[12] = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
    if (n != 20 || s[4] != '-' || s[7] != '-' || s[10] != 'T' || s[13] != ':' || s[16] != ':' ||
        s[19] != 'Z') {
        return false;
    }
    int y1, y2, mo, d, h, mi, se;
    if (!two(s, &y1) || !two(s + 2, &y2) || !two(s + 5, &mo) || !two(s + 8, &d) ||
        !two(s + 11, &h) || !two(s + 14, &mi) || !two(s + 17, &se)) {
        return false;
    }
    int y = y1 * 100 + y2;
    if (mo < 1 || mo > 12 || d < 1 || h > 23 || mi > 59 || se > 59) {
        return false;
    }
    int dim = month_days[mo - 1] + (mo == 2 && is_leap(y) ? 1 : 0);
    if (d > dim) {
        return false;
    }
    *unix_seconds = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se;
    return true;
}

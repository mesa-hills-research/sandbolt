#include <inttypes.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "dtoa.h"

int main(void)
{
    char op, buf[1200];
    int radix, digits, flags;
    uint64_t bits;
    double d;

    while (scanf(" %c", &op) == 1) {
        if (op == 'd') {
            if (scanf("%" SCNx64 "%d%d%d", &bits, &radix, &digits, &flags) != 4)
                return 1;
            memcpy(&d, &bits, sizeof(d));
            JSDTOATempMem tmp;
            int n = js_dtoa(buf, d, radix, digits, flags, &tmp);
            printf("%d %.*s\n", js_dtoa_max_len(d, radix, digits, flags), n, buf);
        } else if (op == 'a') {
            unsigned int byte;
            if (scanf("%d%d%d", &radix, &flags, &digits) != 3 ||
                digits < 0 || digits >= (int)sizeof(buf))
                return 1;
            for (int i = 0; i < digits; i++) {
                if (scanf("%2x", &byte) != 1)
                    return 1;
                buf[i] = byte;
            }
            buf[digits] = '\0';
            const char *end;
            JSATODTempMem tmp;
            d = js_atod(buf, &end, radix, flags, &tmp);
            memcpy(&bits, &d, sizeof(bits));
            if (isnan(d))
                bits = UINT64_C(0x7ff8000000000000);
            printf("%016" PRIx64 " %td\n", bits, end - buf);
        } else if (op == 'i') {
            int64_t signed_bits;
            size_t n;
            if (scanf("%" SCNx64 "%d%d", &bits, &radix, &flags) != 3)
                return 1;
            memcpy(&signed_bits, &bits, sizeof(signed_bits));
            switch (flags) {
            case 0: n = u32toa(buf, (uint32_t)bits); break;
            case 1: n = i32toa(buf, (int32_t)signed_bits); break;
            case 2: n = u64toa(buf, bits); break;
            case 3: n = i64toa(buf, signed_bits); break;
            case 4: n = u64toa_radix(buf, bits, radix); break;
            case 5: n = i64toa_radix(buf, signed_bits, radix); break;
            default: return 1;
            }
            printf("%.*s\n", (int)n, buf);
        } else {
            return 1;
        }
    }
    return ferror(stdin) != 0 || fflush(stdout) != 0;
}

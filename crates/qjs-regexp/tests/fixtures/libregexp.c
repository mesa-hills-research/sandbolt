#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "quickjs.h"
#include "libregexp.h"

static unsigned hex_digit(char c)
{
    return c >= 'a' ? c - 'a' + 10 : c - '0';
}

static unsigned hex(const char **s, int n)
{
    unsigned val = 0;
    while (n-- > 0)
        val = val * 16 + hex_digit(*(*s)++);
    return val;
}

int main(int argc, char **argv)
{
    if (argc != 6)
        return 2;
    int flags = atoi(argv[1]);
    int start = atoi(argv[2]);
    int wide = atoi(argv[3]);
    const char *p = argv[4], *s = argv[5];
    int plen = strlen(p) / 2;
    int slen = strlen(s) / (wide ? 4 : 2);
    char *pattern = calloc(plen + 1, 1);
    void *subject = calloc(slen + 1, wide ? sizeof(uint16_t) : 1);
    if (!pattern || !subject)
        return 2;
    for (int i = 0; i < plen; i++)
        pattern[i] = hex(&p, 2);
    for (int i = 0; i < slen; i++) {
        if (wide)
            ((uint16_t *)subject)[i] = hex(&s, 4);
        else
            ((uint8_t *)subject)[i] = hex(&s, 2);
    }
    JSRuntime *rt = JS_NewRuntime();
    if (!rt)
        return 2;
    JSContext *ctx = JS_NewContext(rt);
    if (!ctx)
        return 2;
    char error[128];
    int len;
    uint8_t *bc = lre_compile(&len, error, sizeof(error), pattern, plen, flags, ctx);
    if (!bc) {
        printf("error %s\n", error);
    } else {
        for (int i = 0; i < len; i++)
            printf("%02x", bc[i]);
        putchar('\n');
        uint8_t *captures[765];
        int result = lre_exec(captures, bc, subject, start, slen, wide, ctx);
        if (result == 1) {
            for (int i = 0; i < lre_get_capture_count(bc); i++) {
                uint8_t *lo = captures[i * 2], *hi = captures[i * 2 + 1];
                if (lo && hi)
                    printf(" %td:%td", (lo - (uint8_t *)subject) >> wide,
                           (hi - (uint8_t *)subject) >> wide);
                else
                    printf(" -");
            }
            putchar('\n');
        } else {
            printf("result %d\n", result);
        }
        lre_realloc(ctx, bc, 0);
    }
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
    free(pattern);
    free(subject);
    return 0;
}

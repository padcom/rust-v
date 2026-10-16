/* Board support for CoreMark on risc-vm. */
#include <stddef.h>

void uart_send_char(char c) { *(volatile unsigned char *)0x10000000 = c; }

void *memset(void *d, int c, size_t n)
{
    unsigned char *p = d;
    while (n--)
        *p++ = (unsigned char)c;
    return d;
}

void *memcpy(void *d, const void *s, size_t n)
{
    unsigned char *p = d;
    const unsigned char *q = s;
    while (n--)
        *p++ = *q++;
    return d;
}

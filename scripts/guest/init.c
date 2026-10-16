/*
 * A minimal freestanding /init for early bring-up: no libc, RV64IMAC only, raw Linux syscalls.
 * Mounts /proc and /sys and runs a tiny command loop on /dev/console.
 */

typedef unsigned long size_t;
typedef long ssize_t;

#define SYS_mkdirat 34
#define SYS_mount 40
#define SYS_openat 56
#define SYS_close 57
#define SYS_getdents64 61
#define SYS_read 63
#define SYS_write 64
#define SYS_exit 93
#define SYS_reboot 142
#define SYS_uname 160

#define AT_FDCWD -100
#define O_RDONLY 0
#define O_DIRECTORY 0200000

#define LINUX_REBOOT_MAGIC1 0xfee1dead
#define LINUX_REBOOT_MAGIC2 672274793
#define LINUX_REBOOT_CMD_RESTART 0x01234567
#define LINUX_REBOOT_CMD_POWER_OFF 0x4321fedc

static long syscall6(long n, long a0, long a1, long a2, long a3, long a4, long a5)
{
    register long r_a0 asm("a0") = a0;
    register long r_a1 asm("a1") = a1;
    register long r_a2 asm("a2") = a2;
    register long r_a3 asm("a3") = a3;
    register long r_a4 asm("a4") = a4;
    register long r_a5 asm("a5") = a5;
    register long r_a7 asm("a7") = n;
    asm volatile("ecall"
                 : "+r"(r_a0)
                 : "r"(r_a1), "r"(r_a2), "r"(r_a3), "r"(r_a4), "r"(r_a5), "r"(r_a7)
                 : "memory");
    return r_a0;
}

#define syscall3(n, a, b, c) syscall6(n, (long)(a), (long)(b), (long)(c), 0, 0, 0)

static size_t strlen(const char *s)
{
    size_t n = 0;
    while (s[n])
        n++;
    return n;
}

static int streq(const char *a, const char *b)
{
    while (*a && *a == *b)
        a++, b++;
    return *a == *b;
}

static void puts_(const char *s) { syscall3(SYS_write, 1, s, strlen(s)); }

static void putdec(unsigned long v)
{
    char buf[24];
    int i = sizeof(buf);
    buf[--i] = 0;
    do {
        buf[--i] = '0' + v % 10;
        v /= 10;
    } while (v);
    puts_(&buf[i]);
}

static void cat(const char *path)
{
    long fd = syscall6(SYS_openat, AT_FDCWD, (long)path, O_RDONLY, 0, 0, 0);
    if (fd < 0) {
        puts_("cat: cannot open ");
        puts_(path);
        puts_("\n");
        return;
    }
    char buf[512];
    long n;
    while ((n = syscall3(SYS_read, fd, buf, sizeof(buf))) > 0)
        syscall3(SYS_write, 1, buf, n);
    syscall3(SYS_close, fd, 0, 0);
}

struct linux_dirent64 {
    unsigned long d_ino;
    long d_off;
    unsigned short d_reclen;
    unsigned char d_type;
    char d_name[];
};

static void ls(const char *path)
{
    long fd = syscall6(SYS_openat, AT_FDCWD, (long)path, O_RDONLY | O_DIRECTORY, 0, 0, 0);
    if (fd < 0) {
        puts_("ls: cannot open ");
        puts_(path);
        puts_("\n");
        return;
    }
    char buf[1024] __attribute__((aligned(8)));
    long n;
    while ((n = syscall3(SYS_getdents64, fd, buf, sizeof(buf))) > 0) {
        for (long off = 0; off < n;) {
            struct linux_dirent64 *d = (struct linux_dirent64 *)(buf + off);
            puts_(d->d_name);
            puts_(d->d_type == 4 ? "/\n" : "\n");
            off += d->d_reclen;
        }
    }
    syscall3(SYS_close, fd, 0, 0);
}

struct utsname {
    char sysname[65], nodename[65], release[65], version[65], machine[65], domainname[65];
};

static void uname(void)
{
    struct utsname u;
    syscall3(SYS_uname, &u, 0, 0);
    puts_(u.sysname);
    puts_(" ");
    puts_(u.release);
    puts_(" ");
    puts_(u.version);
    puts_(" ");
    puts_(u.machine);
    puts_("\n");
}

static void reboot(long cmd)
{
    syscall6(SYS_reboot, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2, cmd, 0, 0, 0);
    puts_("reboot failed\n");
}

static void help(void)
{
    puts_("commands: help, echo ARGS, uname, cat FILE, ls [DIR], cpuinfo, meminfo, uptime,\n"
          "          interrupts, poweroff, reboot\n");
}

static void run(char *line)
{
    char *arg = line;
    while (*arg && *arg != ' ')
        arg++;
    if (*arg)
        *arg++ = 0;
    while (*arg == ' ')
        arg++;

    if (!*line)
        return;
    else if (streq(line, "help"))
        help();
    else if (streq(line, "echo")) {
        puts_(arg);
        puts_("\n");
    } else if (streq(line, "uname"))
        uname();
    else if (streq(line, "cat"))
        cat(arg);
    else if (streq(line, "ls"))
        ls(*arg ? arg : "/");
    else if (streq(line, "cpuinfo"))
        cat("/proc/cpuinfo");
    else if (streq(line, "meminfo"))
        cat("/proc/meminfo");
    else if (streq(line, "uptime"))
        cat("/proc/uptime");
    else if (streq(line, "interrupts"))
        cat("/proc/interrupts");
    else if (streq(line, "poweroff"))
        reboot(LINUX_REBOOT_CMD_POWER_OFF);
    else if (streq(line, "reboot"))
        reboot(LINUX_REBOOT_CMD_RESTART);
    else {
        puts_(line);
        puts_(": unknown command (try help)\n");
    }
}

int main(void)
{
    syscall6(SYS_mount, (long)"proc", (long)"/proc", (long)"proc", 0, 0, 0);
    syscall6(SYS_mount, (long)"sysfs", (long)"/sys", (long)"sysfs", 0, 0, 0);

    puts_("\nrisc-vm init: ");
    uname();
    help();

    char line[256];
    for (;;) {
        puts_("# ");
        long n = syscall3(SYS_read, 0, line, sizeof(line) - 1);
        if (n <= 0)
            continue;
        line[n] = 0;
        if (line[n - 1] == '\n')
            line[n - 1] = 0;
        run(line);
    }
}

asm(".section .text._start\n"
    ".globl _start\n"
    "_start:\n"
    "  call main\n"
    "  li a7, 93\n"
    "  ecall\n");

/* The compiler may emit calls to these even in freestanding code. */
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

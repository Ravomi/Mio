/*
 * crash_probe.c - test-only LD_PRELOAD shim for the crash-recovery acceptance
 * script (tests/crash_recovery.py).
 *
 * It is NOT part of the product and is never linked into Mio. The acceptance
 * script compiles it into a temporary directory at run time and only then
 * preloads it into a throw-away Mio process.
 *
 * The shim gives the test an externally observable, deterministic pause point
 * immediately BEFORE a real libc call executes, so the test can deliver a real
 * SIGKILL at that exact window instead of guessing from wall-clock timing:
 *
 *   connect()  -> pause before the Nth TCP connect (used to intercept a model
 *                 request before it is sent, and the follow-up request after a
 *                 tool result was returned and recorded).
 *   fsync()    -> pause before a file sync whose descriptor resolves to a path
 *                 containing a substring. Session saves call sync_all()
 *                 immediately before the atomic replace; tempfile::persist then
 *                 performs the rename through rustix raw syscalls, which an
 *                 LD_PRELOAD interposer cannot see. Pausing on fsync therefore
 *                 blocks the real pre-replace window of that temp file.
 *   rename()   -> fallback pause before a rename whose destination matches a
 *                 substring, for builds that reach rename through libc.
 *
 * When a pause point triggers, the shim atomically creates the file named by
 * MIO_CRASH_PROBE_READY, writes the point label and detail into it, then spins
 * until it is killed, a release file appears, or MIO_CRASH_PROBE_HOLD_MS
 * elapses (default 60000 ms). The bounded hold means a buggy test can never
 * leak a permanently blocked process.
 *
 * Environment variables (all optional; the shim is inert without them):
 *   MIO_CRASH_PROBE_READY         path of the "paused" marker file to create
 *   MIO_CRASH_PROBE_RELEASE       release file that lets the process continue
 *   MIO_CRASH_PROBE_HOLD_MS       maximum hold time in ms (default 60000)
 *   MIO_CRASH_PROBE_CONNECT_HIT  pause before the Nth matching connect (1-based)
 *   MIO_CRASH_PROBE_CONNECT_PORT only count IPv4/IPv6 connects to this port
 *   MIO_CRASH_PROBE_FSYNC_MATCH  pause before fsync() when the fd path contains this
 *   MIO_CRASH_PROBE_RENAME_MATCH pause before rename() when newpath contains this
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

static int (*real_connect)(int, const struct sockaddr *, socklen_t) = NULL;
static int (*real_fsync)(int) = NULL;
static int (*real_rename)(const char *, const char *) = NULL;
static int (*real_renameat)(int, const char *, int, const char *) = NULL;

static int connect_hits = 0;

__attribute__((constructor)) static void crash_probe_init(void) {
    real_connect = (int (*)(int, const struct sockaddr *, socklen_t))dlsym(RTLD_NEXT, "connect");
    real_fsync = (int (*)(int))dlsym(RTLD_NEXT, "fsync");
    real_rename = (int (*)(const char *, const char *))dlsym(RTLD_NEXT, "rename");
    real_renameat =
        (int (*)(int, const char *, int, const char *))dlsym(RTLD_NEXT, "renameat");
}

static long probe_hold_ms(void) {
    const char *raw = getenv("MIO_CRASH_PROBE_HOLD_MS");
    if (raw == NULL) {
        return 60000;
    }
    char *end = NULL;
    long value = strtol(raw, &end, 10);
    if (end == raw || value < 0) {
        return 60000;
    }
    return value;
}

/* Write the ready file, then block until released, killed, or held too long. */
static void probe_pause(const char *point, const char *detail) {
    const char *ready = getenv("MIO_CRASH_PROBE_READY");
    if (ready == NULL || ready[0] == '\0') {
        return;
    }

    int fd = open(ready, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd >= 0) {
        char line[1024];
        int written = snprintf(line, sizeof(line), "%s\t%s\n", point, detail ? detail : "");
        if (written > 0) {
            ssize_t ignored = write(fd, line, (size_t)written);
            (void)ignored;
        }
        close(fd);
    }

    const char *release = getenv("MIO_CRASH_PROBE_RELEASE");
    long hold_ms = probe_hold_ms();
    struct timespec slice = {0, 10 * 1000 * 1000}; /* 10 ms */
    for (long waited = 0; waited <= hold_ms; waited += 10) {
        if (release != NULL && release[0] != '\0' && access(release, F_OK) == 0) {
            break;
        }
        nanosleep(&slice, NULL);
    }
}

static int probe_port_matches(const struct sockaddr *addr) {
    const char *raw = getenv("MIO_CRASH_PROBE_CONNECT_PORT");
    if (raw == NULL || raw[0] == '\0') {
        return 1;
    }
    long want = strtol(raw, NULL, 10);
    if (addr == NULL) {
        return 0;
    }
    if (addr->sa_family == AF_INET) {
        const struct sockaddr_in *in = (const struct sockaddr_in *)addr;
        return ntohs(in->sin_port) == (uint16_t)want;
    }
    if (addr->sa_family == AF_INET6) {
        const struct sockaddr_in6 *in6 = (const struct sockaddr_in6 *)addr;
        return ntohs(in6->sin6_port) == (uint16_t)want;
    }
    return 0;
}

int connect(int sockfd, const struct sockaddr *addr, socklen_t addrlen) {
    const char *raw_hit = getenv("MIO_CRASH_PROBE_CONNECT_HIT");
    if (raw_hit != NULL && raw_hit[0] != '\0' && probe_port_matches(addr)) {
        long want = strtol(raw_hit, NULL, 10);
        int hit = __sync_add_and_fetch(&connect_hits, 1);
        if (want > 0 && hit == (int)want) {
            probe_pause("connect", "model request");
        }
    }
    if (real_connect == NULL) {
        real_connect =
            (int (*)(int, const struct sockaddr *, socklen_t))dlsym(RTLD_NEXT, "connect");
    }
    return real_connect(sockfd, addr, addrlen);
}

int fsync(int fd) {
    const char *match = getenv("MIO_CRASH_PROBE_FSYNC_MATCH");
    if (match != NULL && match[0] != '\0') {
        char link[4096];
        char target[4096];
        int written = snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
        if (written > 0) {
            ssize_t size = readlink(link, target, sizeof(target) - 1);
            if (size > 0) {
                target[size] = '\0';
                if (strstr(target, match) != NULL) {
                    probe_pause("fsync", target);
                }
            }
        }
    }
    if (real_fsync == NULL) {
        real_fsync = (int (*)(int))dlsym(RTLD_NEXT, "fsync");
    }
    return real_fsync(fd);
}

int rename(const char *oldpath, const char *newpath) {
    const char *match = getenv("MIO_CRASH_PROBE_RENAME_MATCH");
    if (match != NULL && match[0] != '\0' && newpath != NULL && strstr(newpath, match) != NULL) {
        probe_pause("rename", newpath);
    }
    if (real_rename == NULL) {
        real_rename = (int (*)(const char *, const char *))dlsym(RTLD_NEXT, "rename");
    }
    return real_rename(oldpath, newpath);
}

int renameat(int olddirfd, const char *oldpath, int newdirfd, const char *newpath) {
    const char *match = getenv("MIO_CRASH_PROBE_RENAME_MATCH");
    if (match != NULL && match[0] != '\0' && newpath != NULL && strstr(newpath, match) != NULL) {
        probe_pause("renameat", newpath);
    }
    if (real_renameat == NULL) {
        real_renameat = (int (*)(int, const char *, int, const char *))dlsym(RTLD_NEXT, "renameat");
    }
    return real_renameat(olddirfd, oldpath, newdirfd, newpath);
}

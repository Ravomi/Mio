/*
 * workspace_path_swap.c - test-only LD_PRELOAD shim for
 * tests/workspace_path_swap.rs.
 *
 * It is NOT part of the product and is never linked into Mio. The Rust test
 * compiles it with `cc -shared -fPIC ... -ldl` into its own temporary directory
 * and preloads it ONLY into the exact child test process it spawns
 * (`LD_PRELOAD` is set through `Command::env` for that one child). It is never
 * exported into the test process or the ambient environment.
 *
 * Purpose: give the test a deterministic, externally observable pause point
 * BEFORE the final read-only syscall of the workspace read path resolves the
 * target leaf. The parent then swaps the authorised parent directory for a
 * symlink to an out-of-workspace sentinel directory and releases the child, so
 * the read must only ever observe the originally authorised inode, never the
 * sentinel. This is an exact fault point, not probabilistic thread pressure.
 *
 * Intercepted calls (all forwarded to the real symbol unchanged):
 *   open / open64       - the absolute string-path open used today
 *   openat / openat64   - the pinned `openat(parent_fd, "leaf", ...)` a future
 *                         directory-handle implementation uses
 *
 * Matching is intentionally narrow so the pause lands on the FINAL read of the
 * target leaf, never on a metadata probe:
 *   - the basename of the path must equal MIO_SWAP_LEAF, or the full path must
 *     equal MIO_SWAP_ABS (so both "abs" and bare "leaf" are recognised);
 *   - the access mode must be read-only (O_RDONLY) and must not be O_PATH;
 *   - O_DIRECTORY opens are ignored (they only navigate the pinned tree);
 *   - /proc, /sys and /dev are ignored;
 *   - only the first matching open in the process pauses.
 *
 * O_CLOEXEC is required as well: every read open in this codebase (Rust std
 * `File::open` / `OpenOptions` and `src/workspace_fs.rs`) sets it, while any
 * internal libc `realpath` walk would not, so a canonicalize probe can never be
 * mistaken for the read under test.
 *
 * Protocol (private pipes, both ends inherited by the child by design):
 *   MIO_SWAP_ACK_FD       write end the shim uses to ACK the parked fault point
 *   MIO_SWAP_RELEASE_FD   read end the shim blocks on until the parent releases
 *   MIO_SWAP_RELEASE_TIMEOUT_MS  bounded wait (default 15000 ms) so a dead or
 *                         buggy parent can never park the child forever
 *
 * When neither MIO_SWAP_LEAF nor MIO_SWAP_ABS is set the shim is fully inert.
 *
 * FD_CLOEXEC: the Rust harness creates both pipes with O_CLOEXEC and clears
 * FD_CLOEXEC only on the two child-side ends for the duration of the spawn, so
 * the parent's ends and every other descriptor stay close-on-exec. The shim
 * itself never opens a file, so it cannot re-enter the intercepted wrappers.
 *
 * Signals: the shim only writes one ACK byte and reads one release byte. Rust
 * installs SIGPIPE=SIG_IGN in the child, so a vanished parent yields EPIPE
 * rather than killing the child, and the release wait is bounded regardless.
 */

#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

typedef int (*open_fn)(const char *, int, ...);
typedef int (*openat_fn)(int, const char *, int, ...);

static open_fn real_open;
static open_fn real_open64;
static openat_fn real_openat;
static openat_fn real_openat64;

static const char *target_absolute;
static const char *target_leaf;
static int ack_fd = -1;
static int release_fd = -1;
static int release_timeout_ms = 15000;
static int shim_armed;
static int shim_fired;

__attribute__((constructor)) static void workspace_path_swap_init(void) {
    real_open = (open_fn)dlsym(RTLD_NEXT, "open");
    real_open64 = (open_fn)dlsym(RTLD_NEXT, "open64");
    real_openat = (openat_fn)dlsym(RTLD_NEXT, "openat");
    real_openat64 = (openat_fn)dlsym(RTLD_NEXT, "openat64");

    const char *leaf = getenv("MIO_SWAP_LEAF");
    const char *absolute = getenv("MIO_SWAP_ABS");
    const char *ack = getenv("MIO_SWAP_ACK_FD");
    const char *release = getenv("MIO_SWAP_RELEASE_FD");
    const char *timeout = getenv("MIO_SWAP_RELEASE_TIMEOUT_MS");

    if ((leaf == NULL || leaf[0] == '\0') &&
        (absolute == NULL || absolute[0] == '\0')) {
        return;
    }
    target_leaf = (leaf != NULL && leaf[0] != '\0') ? leaf : NULL;
    target_absolute =
        (absolute != NULL && absolute[0] != '\0') ? absolute : NULL;
    if (ack != NULL && ack[0] != '\0') {
        ack_fd = (int)strtol(ack, NULL, 10);
    }
    if (release != NULL && release[0] != '\0') {
        release_fd = (int)strtol(release, NULL, 10);
    }
    if (timeout != NULL && timeout[0] != '\0') {
        long parsed = strtol(timeout, NULL, 10);
        if (parsed > 0) {
            release_timeout_ms = (int)parsed;
        }
    }
    shim_armed = ack_fd >= 0 && release_fd >= 0;
}

static int leaf_matches(const char *path) {
    if (target_absolute != NULL && strcmp(path, target_absolute) == 0) {
        return 1;
    }
    if (target_leaf == NULL) {
        return 0;
    }
    const char *slash = strrchr(path, '/');
    const char *base = slash != NULL ? slash + 1 : path;
    return strcmp(base, target_leaf) == 0;
}

static int is_injected_target(const char *path, int flags) {
    if (!shim_armed || path == NULL) {
        return 0;
    }
    /* Only the final read-only leaf open, not metadata or tree navigation. */
    if ((flags & O_PATH) != 0 || (flags & O_DIRECTORY) != 0) {
        return 0;
    }
    if ((flags & O_ACCMODE) != O_RDONLY) {
        return 0;
    }
    if ((flags & O_CLOEXEC) == 0) {
        return 0;
    }
    if (path[0] == '/') {
        if (strncmp(path, "/proc/", 6) == 0 || strncmp(path, "/sys/", 5) == 0 ||
            strncmp(path, "/dev/", 5) == 0) {
            return 0;
        }
    }
    return leaf_matches(path);
}

/* ACK the parent, then park until released, killed, or bounded out. */
static void swap_pause(void) {
    if (!__sync_bool_compare_and_swap(&shim_fired, 0, 1)) {
        return;
    }
    char ack = 'A';
    ssize_t ignored = write(ack_fd, &ack, 1);
    (void)ignored;

    struct pollfd waiter;
    waiter.fd = release_fd;
    waiter.events = POLLIN;
    waiter.revents = 0;

    int waited = 0;
    while (waited < release_timeout_ms) {
        int ready = poll(&waiter, 1, 100);
        if (ready > 0) {
            if ((waiter.revents & (POLLIN | POLLHUP | POLLERR)) != 0) {
                char release;
                ssize_t got = read(release_fd, &release, 1);
                (void)got;
                return;
            }
        } else if (ready < 0 && errno != EINTR) {
            return;
        }
        waited += 100;
    }
}

static mode_t take_mode(int flags, va_list args) {
    /* The real syscall ignores mode unless the access needs it; forward it for
     * O_CREAT and O_TMPFILE so the wrapper never changes real open semantics. */
    if ((flags & (O_CREAT | O_TMPFILE)) != 0) {
        return (mode_t)va_arg(args, mode_t);
    }
    return 0;
}

int open(const char *path, int flags, ...) {
    va_list args;
    va_start(args, flags);
    mode_t mode = take_mode(flags, args);
    va_end(args);
    if (is_injected_target(path, flags)) {
        swap_pause();
    }
    if (real_open == NULL) {
        real_open = (open_fn)dlsym(RTLD_NEXT, "open");
    }
    return real_open(path, flags, mode);
}

int open64(const char *path, int flags, ...) {
    va_list args;
    va_start(args, flags);
    mode_t mode = take_mode(flags, args);
    va_end(args);
    if (is_injected_target(path, flags)) {
        swap_pause();
    }
    if (real_open64 == NULL) {
        real_open64 = (open_fn)dlsym(RTLD_NEXT, "open64");
    }
    return real_open64(path, flags, mode);
}

int openat(int dirfd, const char *path, int flags, ...) {
    va_list args;
    va_start(args, flags);
    mode_t mode = take_mode(flags, args);
    va_end(args);
    if (is_injected_target(path, flags)) {
        swap_pause();
    }
    if (real_openat == NULL) {
        real_openat = (openat_fn)dlsym(RTLD_NEXT, "openat");
    }
    return real_openat(dirfd, path, flags, mode);
}

int openat64(int dirfd, const char *path, int flags, ...) {
    va_list args;
    va_start(args, flags);
    mode_t mode = take_mode(flags, args);
    va_end(args);
    if (is_injected_target(path, flags)) {
        swap_pause();
    }
    if (real_openat64 == NULL) {
        real_openat64 = (openat_fn)dlsym(RTLD_NEXT, "openat64");
    }
    return real_openat64(dirfd, path, flags, mode);
}

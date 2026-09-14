#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>

/* Linux diagnostic only: reserve exactly the new write range before pwrite.
 * No chunk rounding beyond EOF, no DB code/format changes, no fallback on
 * allocation failure. fstat on every write deliberately avoids stale fd/size
 * caches; its overhead is INCLUDED in timing. Not a physical quota guarantee.
 * Supported experiment targets are 64-bit Linux (Pi aarch64, server x86_64).
 */
_Static_assert(sizeof(off_t) == 8, "64-bit offsets required");
static pthread_once_t once = PTHREAD_ONCE_INIT;
static ssize_t (*next_write)(int, const void *, size_t, off64_t);
static _Atomic uint64_t calls, reservations, requested, failures;
static void init(void) {
    *(void **)(&next_write) = dlsym(RTLD_NEXT, "pwrite64");
    if (!next_write) _exit(125);
}
static ssize_t reserved_write(int fd, const void *buf, size_t len, off64_t off) {
    pthread_once(&once, init);
    atomic_fetch_add(&calls, 1);
    if (off < 0 || len > INT64_MAX || (uint64_t)off > INT64_MAX - len) {
        errno = EINVAL; return -1;
    }
    struct stat s;
    if (fstat(fd, &s)) return -1;
    off64_t end = off + (off64_t)len;
    if (len && S_ISREG(s.st_mode) && end > s.st_size) {
        int rc;
        do { rc = fallocate(fd, FALLOC_FL_KEEP_SIZE, s.st_size, end - s.st_size); }
        while (rc && errno == EINTR);
        if (rc) { atomic_fetch_add(&failures, 1); return -1; }
        atomic_fetch_add(&reservations, 1);
        atomic_fetch_add(&requested, end - s.st_size);
    }
    return next_write(fd, buf, len, off);
}
ssize_t pwrite(int fd, const void *buf, size_t len, off_t off) {
    return reserved_write(fd, buf, len, off);
}
ssize_t pwrite64(int fd, const void *buf, size_t len, off64_t off) {
    return reserved_write(fd, buf, len, off);
}
__attribute__((destructor)) static void finish(void) {
    fprintf(stderr, "{\"allocation_interposer\":true,\"pwrite_calls\":%"PRIu64
            ",\"reservations\":%"PRIu64",\"requested_bytes\":%"PRIu64
            ",\"reservation_failures\":%"PRIu64"}\n",
            atomic_load(&calls), atomic_load(&reservations),
            atomic_load(&requested), atomic_load(&failures));
}

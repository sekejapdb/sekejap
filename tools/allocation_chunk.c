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

/* Diagnostic upper bound on the benefit of batched allocation. NOT an engine
 * policy: one MiB reservation can exceed its logical allowance. Only run on
 * fresh uncapped benchmark files with independently available disk space.
 * Fixed 256-fd cache; higher fds use exact-range reservation. Size/identity
 * checked on every call. Truncate/close invalidate cached reservation.
 */
_Static_assert(sizeof(off_t) == 8, "64-bit Linux only");
enum { CHUNK = 1024 * 1024, FDS = 256 };
struct entry { dev_t dev; ino_t ino; off64_t len, reserved; };
static struct entry entries[FDS];
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static ssize_t (*next_write)(int, const void *, size_t, off64_t);
static int (*next_truncate)(int, off64_t);
static int (*next_close)(int);
static _Atomic uint64_t calls, reservations, requested, failures;
static void init(void) {
    *(void **)(&next_write) = dlsym(RTLD_NEXT, "pwrite64");
    *(void **)(&next_truncate) = dlsym(RTLD_NEXT, "ftruncate64");
    *(void **)(&next_close) = dlsym(RTLD_NEXT, "close");
    if (!next_write || !next_truncate || !next_close) _exit(125);
}
static ssize_t write_reserved(int fd, const void *buf, size_t len, off64_t off) {
    pthread_once(&once, init);
    atomic_fetch_add(&calls, 1);
    if (off < 0 || len > INT64_MAX - CHUNK || (uint64_t)off > INT64_MAX - len - CHUNK) {
        errno = EINVAL; return -1;
    }
    pthread_mutex_lock(&lock);
    struct stat s;
    ssize_t result = -1;
    if (fstat(fd, &s)) goto finish;
    struct entry fallback = {0};
    struct entry *e = fd >= 0 && fd < FDS ? &entries[fd] : &fallback;
    if (e->dev != s.st_dev || e->ino != s.st_ino || e->len != s.st_size)
        *e = (struct entry){s.st_dev, s.st_ino, s.st_size, s.st_size};
    off64_t end = off + (off64_t)len;
    if (len && S_ISREG(s.st_mode) && end > e->reserved) {
        off64_t target = e == &fallback ? end : (end + CHUNK - 1) / CHUNK * CHUNK;
        int rc;
        do { rc = fallocate(fd, FALLOC_FL_KEEP_SIZE, e->reserved, target - e->reserved); }
        while (rc && errno == EINTR);
        if (rc) { atomic_fetch_add(&failures, 1); goto finish; }
        atomic_fetch_add(&reservations, 1);
        atomic_fetch_add(&requested, target - e->reserved);
        e->reserved = target;
    }
    result = next_write(fd, buf, len, off);
    if (result > 0 && off + result > e->len) e->len = off + result;
finish:;
    int saved = errno;
    pthread_mutex_unlock(&lock);
    errno = saved;
    return result;
}
ssize_t pwrite(int fd, const void *b, size_t n, off_t off) { return write_reserved(fd,b,n,off); }
ssize_t pwrite64(int fd, const void *b, size_t n, off64_t off) { return write_reserved(fd,b,n,off); }
static int truncate_file(int fd, off64_t n) {
    pthread_once(&once, init);
    pthread_mutex_lock(&lock);
    if (fd >= 0 && fd < FDS) entries[fd] = (struct entry){0};
    int rc = next_truncate(fd,n), saved = errno;
    pthread_mutex_unlock(&lock);
    errno = saved; return rc;
}
int ftruncate(int fd, off_t n) { return truncate_file(fd,n); }
int ftruncate64(int fd, off64_t n) { return truncate_file(fd,n); }
int close(int fd) {
    pthread_once(&once, init);
    pthread_mutex_lock(&lock);
    if (fd >= 0 && fd < FDS) entries[fd] = (struct entry){0};
    int rc = next_close(fd), saved = errno;
    pthread_mutex_unlock(&lock);
    errno = saved; return rc;
}
__attribute__((destructor)) static void finish(void) {
    fprintf(stderr, "{\"allocation_chunk\":%d,\"pwrite_calls\":%"PRIu64
            ",\"reservations\":%"PRIu64",\"requested_bytes\":%"PRIu64
            ",\"reservation_failures\":%"PRIu64"}\n", CHUNK,
            atomic_load(&calls), atomic_load(&reservations),
            atomic_load(&requested), atomic_load(&failures));
}

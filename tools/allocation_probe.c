#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/fiemap.h>
#include <linux/fs.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

/* Standalone Linux allocation control. No E4/SQLite code or direct I/O.
 * Only a fresh O_EXCL file under an explicitly authorized artifact root.
 * Measures after each operation; these are boundary samples, not a hard cap.
 * Diagnostic timings would include measurement overhead; not a speed test. */
enum { PAGE = 4096, CHUNK = 256 * PAGE, BASE_PAGES = 24576, ROUNDS = 12 };
static uint64_t peak, phase_peak;
static int mode;
static off_t reserved;

static void die(const char *op) { perror(op); exit(1); }
static void measure(int fd) {
    struct stat s;
    if (fstat(fd, &s)) die("fstat");
    uint64_t bytes = (uint64_t)s.st_blocks * 512;
    if (bytes > peak) peak = bytes;
    if (bytes > phase_peak) phase_peak = bytes;
}
static void report(int fd, const char *stage, int round) {
    struct stat s;
    if (fstat(fd, &s)) die("fstat report");
    uint64_t allocated = (uint64_t)s.st_blocks * 512;
    if (allocated > peak) peak = allocated;
    if (allocated > phase_peak) phase_peak = allocated;
    /* No FIEMAP_FLAG_SYNC: observing extents must not force writeback. */
    unsigned char raw[sizeof(struct fiemap) + 128 * sizeof(struct fiemap_extent)]
        __attribute__((aligned(8))) = {0};
    struct fiemap *map = (struct fiemap *)raw;
    map->fm_length = UINT64_MAX;
    map->fm_extent_count = 128;
    int rc = ioctl(fd, FS_IOC_FIEMAP, map);
    int error = rc ? errno : 0;
    uint64_t extent_end = 0, beyond = 0;
    unsigned flags = 0;
    for (unsigned i = 0; !rc && i < map->fm_mapped_extents; ++i) {
        struct fiemap_extent *e = &map->fm_extents[i];
        uint64_t end = e->fe_logical + e->fe_length;
        if (end > extent_end) extent_end = end;
        if (end > (uint64_t)s.st_size) {
            uint64_t start = e->fe_logical > (uint64_t)s.st_size ? e->fe_logical : (uint64_t)s.st_size;
            beyond += end - start;
            flags |= e->fe_flags;
        }
    }
    printf("{\"stage\":\"%s\",\"round\":%d,\"logical\":%"PRIu64
           ",\"allocated\":%"PRIu64",\"phase_peak\":%"PRIu64
           ",\"peak\":%"PRIu64",\"fiemap_errno\":%d,\"extent_end\":%"PRIu64
           ",\"beyond_eof_bytes\":%"PRIu64",\"beyond_eof_flags\":%u,\"extent_count\":%u}\n",
           stage, round, (uint64_t)s.st_size, (uint64_t)s.st_blocks * 512,
           phase_peak, peak, error, extent_end, beyond, flags, rc ? 0 : map->fm_mapped_extents);
    fflush(stdout);
}
static void payload(uint64_t *buf, uint64_t page, unsigned generation) {
    for (unsigned w = 0; w < PAGE / 8; ++w)
        buf[w] = page * UINT64_C(0x9e3779b97f4a7c15) ^
                 ((uint64_t)generation << 32) ^ ((uint64_t)w * UINT64_C(0x100000001b3));
}
static void write_page(int fd, uint64_t page, unsigned generation) {
    off_t off = (off_t)page * PAGE;
    if (mode == 1 && off + PAGE > reserved) {
        off_t end = ((off + PAGE + CHUNK - 1) / CHUNK) * CHUNK;
        if (fallocate(fd, FALLOC_FL_KEEP_SIZE, reserved, end - reserved)) die("fallocate KEEP_SIZE");
        reserved = end;
        measure(fd);
    }
    uint64_t buf[PAGE / 8];
    payload(buf, page, generation);
    size_t done = 0;
    while (done != PAGE) {
        ssize_t n = pwrite(fd, (char *)buf + done, PAGE - done, off + done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) die("pwrite");
        done += (size_t)n;
    }
    measure(fd);
}
static void barrier(int fd, off_t end) {
    if (fsync(fd)) die("fsync");
    measure(fd);
    if (mode == 2) {
        if (ftruncate(fd, end)) die("same-size truncate");
        measure(fd);
        if (fsync(fd)) die("trim fsync");
    }
}
int main(int argc, char **argv) {
    if (argc != 3) { fprintf(stderr, "allocation_probe FRESH_FILE plain|reserve|trim\n"); return 2; }
    const char *pi = "<scratch>/";
    const char *server = "<scratch>/";
    if ((strncmp(argv[1], pi, strlen(pi)) && strncmp(argv[1], server, strlen(server))) || strstr(argv[1], "/../")) return 2;
    if (!strcmp(argv[2], "plain")) mode = 0;
    else if (!strcmp(argv[2], "reserve")) mode = 1;
    else if (!strcmp(argv[2], "trim")) mode = 2;
    else return 2;
    int fd = open(argv[1], O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) die("fresh open");
    for (uint64_t p = 0; p < BASE_PAGES; ++p) {
        write_page(fd, p, 0);
        if ((p + 1) % 256 == 0) barrier(fd, (off_t)(p + 1) * PAGE);
    }
    report(fd, "loaded_before_trim", -1);
    if (ftruncate(fd, BASE_PAGES * (off_t)PAGE) || fsync(fd)) die("load checkpoint");
    report(fd, "loaded", -1);
    peak = phase_peak = 0;
    for (unsigned round = 0; round < ROUNDS; ++round) {
        phase_peak = 0;
        for (uint64_t p = 0; p < 64; ++p) write_page(fd, p, round + 1);
        uint64_t begin = BASE_PAGES + round * 256;
        for (uint64_t p = begin; p < begin + 256; ++p) write_page(fd, p, 0);
        report(fd, "before_sync", round);
        barrier(fd, (off_t)(begin + 256) * PAGE);
        report(fd, "after_sync", round);
    }
    report(fd, "before_final_trim", ROUNDS);
    uint64_t pages = BASE_PAGES + ROUNDS * 256;
    if (ftruncate(fd, (off_t)pages * PAGE) || fsync(fd)) die("final trim");
    report(fd, "after_final_trim", ROUNDS);
    if (close(fd)) die("close");
    fd = open(argv[1], O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) die("reopen");
    report(fd, "reopened", ROUNDS);
    uint64_t buf[PAGE / 8], expected[PAGE / 8];
    for (uint64_t p = 0; p < pages; ++p) {
        payload(expected, p, p < 64 ? ROUNDS : 0);
        size_t done = 0;
        while (done != PAGE) {
            ssize_t n = pread(fd, (char *)buf + done, PAGE - done, (off_t)p * PAGE + done);
            if (n < 0 && errno == EINTR) continue;
            if (n <= 0) die("verify pread");
            done += (size_t)n;
        }
        if (memcmp(buf, expected, PAGE)) { fprintf(stderr, "wrong bytes on page %"PRIu64"\n", p); return 1; }
    }
    if (close(fd)) die("verified close");
    printf("{\"verified\":true,\"pages\":%"PRIu64",\"mode\":\"%s\"}\n", pages, argv[2]);
    return 0;
}
